//! 端到端测试：本地限速 Range 服务器（可模拟限流、故障、停滞、镜像）+ 真实下载流程。
use super::*;
use axum::body::Body;
use axum::extract::Path as AxPath;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::get;
use futures_util::stream;
use std::sync::LazyLock;
use std::sync::atomic::AtomicUsize;

const SIZE: usize = 24 << 20; // 24 MiB
static DATA: LazyLock<Arc<Vec<u8>>> = LazyLock::new(|| {
    let mut x: u64 = 0x9E3779B97F4A7C15;
    Arc::new(
        (0..SIZE)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect(),
    )
});

/// 同时连接数计数（用于模拟“每 IP 最多 2 连接”的服务器）
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static FLAKY_N: AtomicUsize = AtomicUsize::new(0);
static STALLED: AtomicBool = AtomicBool::new(false);

struct ConnGuard;
impl Drop for ConnGuard {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Relaxed);
    }
}

#[derive(Clone, Copy, Default)]
struct Opts {
    /// 发送这么多字节后永久挂起（只触发一次）
    stall_after: Option<usize>,
    /// 计入同时连接数
    count: bool,
    etag: &'static str,
    disposition: bool,
}

/// 每连接约 8 MiB/s 的流式响应
fn slow_body(start: usize, end: usize, o: Opts) -> Body {
    let data = DATA.clone();
    let guard = o.count.then(|| ConnGuard); // 注意：不能用 then_some，否则会提前构造并析构守卫导致计数下溢
    let s = stream::unfold((start, guard), move |(pos, guard)| {
        let data = data.clone();
        async move {
            if pos >= end {
                return None;
            }
            if let Some(n) = o.stall_after
                && pos >= start + n
                && !STALLED.swap(true, Relaxed)
            {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(Duration::from_millis(8)).await;
            let e = (pos + 64 * 1024).min(end);
            Some((Ok::<_, std::io::Error>(data[pos..e].to_vec()), (e, guard)))
        }
    });
    Body::from_stream(s)
}

fn serve_range(h: &HeaderMap, o: Opts) -> Response<Body> {
    let range = h.get("range").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("bytes="));
    let Some(r) = range else {
        return Response::builder().header("content-length", SIZE).body(slow_body(0, SIZE, o)).unwrap();
    };
    let (a, b) = r.split_once('-').unwrap();
    let a: usize = a.parse().unwrap();
    let b: usize = if b.is_empty() { SIZE - 1 } else { b.parse::<usize>().unwrap().min(SIZE - 1) };
    let mut rb = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header("content-range", format!("bytes {a}-{b}/{SIZE}"))
        .header("content-length", b - a + 1)
        .header("etag", o.etag);
    if o.disposition {
        rb = rb.header("content-disposition", "attachment; filename*=UTF-8''%E6%B5%8B%E8%AF%95.bin");
    }
    rb.body(slow_body(a, b + 1, o)).unwrap()
}

fn status(code: u16) -> Response<Body> {
    Response::builder().status(code).body(Body::empty()).unwrap()
}

const BASE: Opts = Opts { stall_after: None, count: false, etag: "\"v1\"", disposition: true };

async fn ranged(h: HeaderMap) -> Response<Body> {
    serve_range(&h, BASE)
}
/// 同一文件的另一个镜像：ETag 不同
async fn mirror(h: HeaderMap) -> Response<Body> {
    serve_range(&h, Opts { etag: "\"mirror\"", ..BASE })
}
/// 文件名只能从 URL 获取
async fn plain(AxPath(_name): AxPath<String>, h: HeaderMap) -> Response<Body> {
    serve_range(&h, Opts { disposition: false, ..BASE })
}
/// 每 3 个分段请求失败 1 个（503）
async fn flaky(h: HeaderMap) -> Response<Body> {
    if h.contains_key("range") && FLAKY_N.fetch_add(1, Relaxed) % 3 == 2 && h["range"] != "bytes=0-0" {
        return status(503);
    }
    serve_range(&h, BASE)
}
/// 最多同时 2 个连接，超出返回 503
async fn limited(h: HeaderMap) -> Response<Body> {
    if h["range"] != "bytes=0-0" {
        if ACTIVE.fetch_add(1, Relaxed) >= 2 {
            ACTIVE.fetch_sub(1, Relaxed);
            return status(503);
        }
        return serve_range(&h, Opts { count: true, ..BASE });
    }
    serve_range(&h, BASE)
}
/// 某个连接发送 512 KiB 后永久挂起
async fn stall(h: HeaderMap) -> Response<Body> {
    let is_probe = h.get("range").is_some_and(|v| v == "bytes=0-0");
    serve_range(&h, Opts { stall_after: (!is_probe).then_some(512 << 10), ..BASE })
}
async fn gone() -> Response<Body> {
    status(403)
}
async fn norange() -> Response<Body> {
    Response::builder().header("content-length", SIZE).body(slow_body(0, SIZE, BASE)).unwrap()
}
async fn chunked() -> Response<Body> {
    Response::builder().body(slow_body(0, SIZE / 4, BASE)).unwrap()
}

// ───────────── HLS 测试服务器 ─────────────
const SEGS: usize = 40;
const HLS_KEY: [u8; 16] = [0x11; 16];

/// 第 i 个分片的明文（不同清晰度内容不同，便于验证选择了哪个）
fn seg_plain(v: &str, i: usize) -> Vec<u8> {
    let off = if v == "low" { 7 } else { 0 };
    let len = 50_000 + i * 37;
    DATA[i * 60_000 + off..i * 60_000 + off + len].to_vec()
}

fn hls_expected(v: &str) -> Vec<u8> {
    let mut out = Vec::new();
    if v == "fmp4" {
        out.extend_from_slice(b"INIT-SEGMENT-ftyp-moov");
    }
    (0..SEGS).for_each(|i| out.extend(seg_plain(v, i)));
    out
}

async fn hls_master() -> String {
    "#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
low/index.m3u8
     #EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720
high/index.m3u8
"
        .into()
}

async fn hls_file(AxPath((v, file)): AxPath<(String, String)>) -> Response<Body> {
    if file == "index.m3u8" {
        let mut t = String::from("#EXTM3U
#EXT-X-TARGETDURATION:4
");
        let ext = if v == "fmp4" { "m4s" } else { "ts" };
        match v.as_str() {
            "enc" | "keybad" => t.push_str("#EXT-X-MEDIA-SEQUENCE:5
#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"
"),
            "fmp4" => t.push_str("#EXT-X-MAP:URI=\"init.mp4\"
"),
            "drm" => t.push_str("#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\",KEYFORMAT=\"com.apple.streamingkeydelivery\"
"),
            _ => {}
        }
        for i in 0..SEGS {
            t.push_str(&format!("#EXTINF:4.0,
seg{i}.{ext}
"));
        }
        if v != "live" {
            t.push_str("#EXT-X-ENDLIST
");
        }
        return Response::builder().header("content-type", "application/vnd.apple.mpegurl").body(Body::from(t)).unwrap();
    }
    if file == "key.bin" {
        return if v == "keybad" { status(404) } else { Response::new(Body::from(HLS_KEY.to_vec())) };
    }
    if file == "init.mp4" {
        return Response::new(Body::from(&b"INIT-SEGMENT-ftyp-moov"[..]));
    }
    let Some(i) = file.strip_prefix("seg").and_then(|f| f.split('.').next()).and_then(|n| n.parse::<usize>().ok()) else {
        return status(404);
    };
    if v == "bad" && i == 7 {
        return status(404);
    }
    tokio::time::sleep(Duration::from_millis(if v == "slow" { 90 } else { 15 })).await;
    let mut data = seg_plain(&v, i);
    if v == "enc" {
        use aes::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
        let iv = ((5 + i) as u128).to_be_bytes();
        data = cbc::Encryptor::<aes::Aes128>::new(&HLS_KEY.into(), &iv.into()).encrypt_padded_vec::<Pkcs7>(&data);
    }
    Response::new(Body::from(data))
}

struct Env {
    rt: tokio::runtime::Runtime,
    base: String,
    dir: PathBuf,
}

fn env(name: &str) -> Env {
    let root = std::env::temp_dir().join(format!("swiftdl-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // 隔离配置目录（测试需 --test-threads=1 串行运行）
    unsafe { std::env::set_var("APPDATA", root.join("appdata")) };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route("/file", get(ranged))
        .route("/mirror", get(mirror))
        .route("/dl/{name}", get(plain))
        .route("/flaky", get(flaky))
        .route("/limited", get(limited))
        .route("/stall", get(stall))
        .route("/gone", get(gone))
        .route("/norange", get(norange))
        .route("/chunked", get(chunked))
        .route("/hls/master.m3u8", get(hls_master))
        .route("/hls/{v}/{file}", get(hls_file));
    rt.spawn(async move { axum::serve(listener, app).await.unwrap() });
    Env { rt, base, dir: root.join("dl") }
}

fn settings(e: &Env) -> Settings {
    Settings { dir: e.dir.clone(), proxy: "direct".into(), max_active: 3, ..Default::default() }
}

fn add(m: &Arc<Manager>, e: &Env, path: &str, name: &str, conns: u32) -> Arc<Task> {
    let id = m.add(NewTask {
        req: AddReq { url: format!("{}{path}", e.base), ..Default::default() },
        name: name.into(),
        dir: e.dir.clone(),
        auto_dir: false,
        conns,
        mode: StartMode::Now,
        sha256: None,
    });
    m.get(id).unwrap()
}

fn wait(t: &Task, want: Status, secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while t.status() != want {
        if std::time::Instant::now() >= deadline {
            let segs: Vec<String> = t
                .segs
                .lock()
                .unwrap()
                .iter()
                .map(|s| {
                    let p = *s.st.lock().unwrap();
                    format!("[{}..{} w={} busy={} fail={}]", p.pos, p.end, s.written.load(Relaxed), s.busy.load(Relaxed), s.fail_rounds.load(Relaxed))
                })
                .collect();
            panic!(
                "timeout waiting {want:?}, now {:?} err={:?} downloaded={} active={} limit={} segs={}",
                t.status(),
                t.meta.lock().unwrap().error,
                t.downloaded.load(Relaxed),
                t.active_conns.load(Relaxed),
                t.conn_limit.load(Relaxed),
                segs.join(" ")
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn check(t: &Task, expect: &[u8]) {
    let got = std::fs::read(t.final_path()).unwrap();
    assert_eq!(got.len(), expect.len(), "length mismatch");
    assert!(got == expect, "content mismatch");
    assert!(!t.part_path().exists());
}

fn is_sparse(p: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::metadata(p).is_ok_and(|m| m.file_attributes() & 0x200 != 0)
}

#[test]
fn end_to_end() {
    let e = env("e2e");
    let m = Manager::new(e.rt.handle().clone(), settings(&e));

    // 1. 多连接 + 动态分段 + Content-Disposition 中文名 + 稀疏预分配
    let t0 = std::time::Instant::now();
    let t = add(&m, &e, "/file", "", 8);
    let mut saw_sparse = false;
    while t.status() != Status::Done && t.status() != Status::Error {
        saw_sparse |= t.meta.lock().unwrap().probed && is_sparse(&t.part_path());
        std::thread::sleep(Duration::from_millis(5));
    }
    wait(&t, Status::Done, 60);
    let el = t0.elapsed();
    check(&t, &DATA);
    assert_eq!(t.meta.lock().unwrap().name, "测试.bin");
    assert!(saw_sparse, "下载中的临时文件应为稀疏文件");
    assert!(!is_sparse(&t.final_path()), "完成后应恢复为普通文件");
    println!("multi-conn 24MiB in {el:?}");

    // 2. 暂停 → 续传，内容必须一致
    let t = add(&m, &e, "/file", "resume.bin", 4);
    std::thread::sleep(Duration::from_millis(600));
    m.pause(t.id);
    wait(&t, Status::Paused, 10);
    let mid = t.downloaded.load(Relaxed);
    assert!(mid > 0 && (mid as usize) < SIZE, "paused at {mid}");
    assert!(t.part_path().exists());
    // 模拟重启：从持久化记录恢复
    m.save();
    let rec = store::load_tasks().into_iter().find(|r| r.id == t.id).unwrap();
    assert_eq!(rec.status, Status::Paused);
    let restored = Task::from_rec(rec);
    assert_eq!(restored.downloaded.load(Relaxed), mid);
    m.start(t.id);
    wait(&t, Status::Done, 60);
    check(&t, &DATA);

    // 3. 不支持 Range：单连接
    let t = add(&m, &e, "/norange", "norange.bin", 8);
    wait(&t, Status::Done, 60);
    check(&t, &DATA);
    assert!(!t.meta.lock().unwrap().resumable);

    // 4. 无 Content-Length
    let t = add(&m, &e, "/chunked", "chunked.bin", 8);
    wait(&t, Status::Done, 60);
    check(&t, &DATA[..SIZE / 4]);

    // 5. 同名文件自动改名
    let t = add(&m, &e, "/file", "resume.bin", 8);
    wait(&t, Status::Done, 60);
    assert_eq!(t.meta.lock().unwrap().name, "resume(1).bin");
    check(&t, &DATA);

    // 6. 限速：稳态约 1 MiB/s
    let cur = m.settings.read().unwrap().clone();
    m.apply_settings(Settings { speed_limit_kb: 1024, ..cur });
    let t = add(&m, &e, "/file", "limited.bin", 8);
    // 跳过起始突发（每连接已在 socket 缓冲中的首块数据），测稳态速率
    std::thread::sleep(Duration::from_millis(1500));
    let a = t.downloaded.load(Relaxed);
    std::thread::sleep(Duration::from_millis(3000));
    let b = t.downloaded.load(Relaxed);
    m.pause(t.id);
    wait(&t, Status::Paused, 10);
    let rate = (b - a) as f64 / 3.0 / 1048576.0;
    println!("limited steady rate = {rate:.2} MiB/s");
    assert!((0.7..1.3).contains(&rate), "rate limit off: {rate:.2} MiB/s");

    // 7. 定期清缓存：孤立 .swdl 文件被删除
    let orphan = e.dir.join("orphan.bin.swdl");
    std::fs::write(&orphan, b"x").unwrap();
    let f = std::fs::File::options().write(true).open(&orphan).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(3600)).unwrap();
    drop(f);
    m.cleanup();
    assert!(!orphan.exists(), "orphan not cleaned");
    assert!(t.part_path().exists(), "active part file must be kept");

    // 8. 删除任务同时删除临时文件
    let part = t.part_path();
    m.remove(&[t.id], true);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!part.exists());

    // 9. 只写入有变化的任务行（SQLite）
    m.save();
    assert_eq!(m.save(), 0, "内容未变化不应写入");
    let second = m.tasks.read().unwrap()[1].id;
    m.move_task(second, Move::Top);
    assert_eq!(m.save(), 2, "调整顺序只应更新两行的位置");
    assert!(store::data_dir().join("swiftdl.db").exists());
}

#[test]
fn robustness() {
    let e = env("robust");
    let m = Manager::new(e.rt.handle().clone(), settings(&e));

    // 1. 故障隔离：1/3 的分段请求返回 503，任务仍应完成
    let t = add(&m, &e, "/flaky", "flaky.bin", 8);
    wait(&t, Status::Done, 40);
    check(&t, &DATA);

}

#[test]
fn adaptive_conns() {
    let e = env("adaptive");
    let m = Manager::new(e.rt.handle().clone(), settings(&e));
    // 2. 自适应并发：服务器最多 2 连接，8 连接任务应自动降速完成
    let t = add(&m, &e, "/limited", "limited.bin", 8);
    wait(&t, Status::Done, 120);
    check(&t, &DATA);
    let lim = t.conn_limit.load(Relaxed);
    assert!(lim <= 3, "限流后连接上限应下调，实际 {lim}");
    println!("adaptive conn limit -> {lim}");

}

#[test]
fn robustness2() {
    let e = env("robust2");
    let m = Manager::new(e.rt.handle().clone(), settings(&e));
    // 3. 停滞检测：某连接中途永久挂起，任务仍应在限定时间内完成
    task::STALL_MS.store(1500, Relaxed);
    let t0 = std::time::Instant::now();
    let t = add(&m, &e, "/stall", "stall.bin", 4);
    wait(&t, Status::Done, 60);
    check(&t, &DATA);
    assert!(STALLED.load(Relaxed), "测试服务器应已触发挂起");
    println!("stalled connection recovered, total {:?}", t0.elapsed());
    task::STALL_MS.store(15_000, Relaxed);

    // 4. 不可恢复错误（403）应快速失败，并给出更新链接提示
    let t0 = std::time::Instant::now();
    let t = add(&m, &e, "/gone", "gone.bin", 8);
    wait(&t, Status::Error, 10);
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert!(t.meta.lock().unwrap().error.clone().unwrap().contains("403"));

    // 5. 更新链接：暂停后换成 ETag 不同的镜像，应从断点续传且内容一致
    let t = add(&m, &e, "/file", "relink.bin", 4);
    std::thread::sleep(Duration::from_millis(500));
    m.pause(t.id);
    wait(&t, Status::Paused, 10);
    let before = t.downloaded.load(Relaxed);
    let r = e.rt.block_on(m.relink(t.id, format!("{}/mirror", e.base)));
    assert!(r.is_ok(), "{r:?}");
    wait(&t, Status::Done, 60);
    check(&t, &DATA);
    assert!(before > 0);
    // 大小不一致的新链接应被拒绝
    let t = add(&m, &e, "/file", "relink2.bin", 4);
    std::thread::sleep(Duration::from_millis(400));
    m.pause(t.id);
    wait(&t, Status::Paused, 10);
    let r = e.rt.block_on(m.relink(t.id, format!("{}/chunked", e.base)));
    assert!(r.is_err(), "大小不一致应拒绝");
    m.remove(&[t.id], false);

    // 6. 按类型分类保存：使用默认目录添加的 .zip 存入 压缩文件 子目录
    let id = m.add(NewTask {
        req: AddReq { url: format!("{}/dl/pack.zip", e.base), ..Default::default() },
        name: String::new(),
        dir: e.dir.clone(),
        auto_dir: true,
        conns: 8,
        mode: StartMode::Now,
        sha256: None,
    });
    let t = m.get(id).unwrap();
    wait(&t, Status::Done, 60);
    assert_eq!(t.final_path(), e.dir.join("压缩文件").join("pack.zip"));
    check(&t, &DATA);

    // 7. SHA-256：正确值通过，错误值标记出错
    let good = {
        use sha2::Digest;
        sha2::Sha256::digest(&**DATA).iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    for (sha, ok) in [(good.to_uppercase(), true), ("00".repeat(32), false)] {
        let id = m.add(NewTask {
            req: AddReq { url: format!("{}/file", e.base), ..Default::default() },
            name: format!("sha-{ok}.bin"),
            dir: e.dir.clone(),
            auto_dir: false,
            conns: 8,
            mode: StartMode::Now,
            sha256: Some(sha),
        });
        let t = m.get(id).unwrap();
        wait(&t, if ok { Status::Done } else { Status::Error }, 60);
        assert_eq!(t.meta.lock().unwrap().hash.as_deref(), Some(good.as_str()));
        if !ok {
            assert!(t.meta.lock().unwrap().error.clone().unwrap().contains("SHA-256"));
        }
    }

    // 8. 计划任务：添加后保持暂停，标记为计划
    let id = m.add(NewTask {
        req: AddReq { url: format!("{}/file", e.base), ..Default::default() },
        name: "planned.bin".into(),
        dir: e.dir.clone(),
        auto_dir: false,
        conns: 4,
        mode: StartMode::Scheduled,
        sha256: None,
    });
    let t = m.get(id).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(t.status(), Status::Paused);
    assert!(t.is_scheduled());
}

#[test]
fn queue_limit() {
    let e = env("queue");
    let s = Settings { max_active: 2, ..settings(&e) };
    let m = Manager::new(e.rt.handle().clone(), s);
    let ts: Vec<Arc<Task>> = (0..5).map(|i| add(&m, &e, "/file", &format!("q{i}.bin"), 2)).collect();
    std::thread::sleep(Duration::from_millis(300));
    let running = m.tasks.read().unwrap().iter().filter(|t| t.status() == Status::Running).count();
    assert_eq!(running, 2);
    for t in &ts {
        wait(t, Status::Done, 120);
    }
}

/// 真实网络下载（手动运行）：
/// SWIFTDL_URL=... SWIFTDL_PROXY=http://127.0.0.1:10808 cargo test --release real_download -- --ignored --nocapture
#[test]
#[ignore]
fn real_download() {
    let url = std::env::var("SWIFTDL_URL").expect("SWIFTDL_URL");
    let proxy = std::env::var("SWIFTDL_PROXY").unwrap_or_default();
    let root = std::env::temp_dir().join("swiftdl-real");
    let _ = std::fs::remove_dir_all(&root);
    unsafe { std::env::set_var("APPDATA", root.join("appdata")) };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let m = Manager::new(rt.handle().clone(), Settings { dir: root.clone(), proxy, ..Default::default() });
    let t0 = std::time::Instant::now();
    let id = m.add(NewTask {
        req: AddReq { url, ..Default::default() },
        name: String::new(),
        dir: root.clone(),
        auto_dir: false,
        conns: 8,
        mode: StartMode::Now,
        sha256: None,
    });
    let t = m.get(id).unwrap();
    wait(&t, Status::Done, 600);
    let size = t.meta.lock().unwrap().size.unwrap();
    let el = t0.elapsed().as_secs_f64();
    println!(
        "OK {} {} bytes in {el:.1}s ({:.1} MiB/s) resumable={}",
        t.final_path().display(),
        size,
        size as f64 / el / 1048576.0,
        t.meta.lock().unwrap().resumable
    );
}

#[test]
fn hls_e2e() {
    let e = env("hls");
    let m = Manager::new(e.rt.handle().clone(), settings(&e));
    let run = |path: &str, name: &str| {
        let t = add(&m, &e, path, name, 4);
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !matches!(t.status(), Status::Done | Status::Error) {
            assert!(std::time::Instant::now() < deadline, "HLS 任务超时");
            std::thread::sleep(Duration::from_millis(10));
        }
        t
    };
    let read = |t: &Task| std::fs::read(t.final_path()).unwrap();

    // 1. 主列表：默认选择最高码率；无 ffmpeg 时保存为 .ts 并说明原因
    let t = run("/hls/master.m3u8", "");
    assert_eq!(t.status(), Status::Done, "{:?}", t.meta.lock().unwrap().error);
    assert!(t.final_path().extension().is_some_and(|x| x == "ts"), "{:?}", t.final_path());
    assert!(read(&t) == hls_expected("high"), "主列表应下载最高清晰度");
    assert!(t.meta.lock().unwrap().hls.as_ref().unwrap().note.as_deref().unwrap_or("").contains("ffmpeg"));
    assert_eq!(t.seg_done.load(Relaxed), SEGS as u32);
    assert!(!t.part_path().exists());

    // 2. 直接给低清晰度媒体列表
    let t = run("/hls/low/index.m3u8", "low.ts");
    assert!(read(&t) == hls_expected("low"));

    // 3. AES-128 加密（IV 由媒体序号推导）
    let t = run("/hls/enc/index.m3u8", "enc.ts");
    assert_eq!(t.status(), Status::Done, "{:?}", t.meta.lock().unwrap().error);
    assert!(read(&t) == hls_expected("enc"), "解密结果应与明文一致");
    assert!(t.meta.lock().unwrap().hls.as_ref().unwrap().encrypted);

    // 4. fMP4：初始化段 + 分片，直接保存为 .mp4
    let t = run("/hls/fmp4/index.m3u8", "clip.mp4");
    assert!(t.final_path().extension().is_some_and(|x| x == "mp4"));
    assert!(read(&t) == hls_expected("fmp4"));

    // 5. 暂停 → 持久化 → 续传，结果一致
    let t = add(&m, &e, "/hls/slow/index.m3u8", "slow.ts", 4);
    std::thread::sleep(Duration::from_millis(500));
    m.pause(t.id);
    wait(&t, Status::Paused, 10);
    let done = t.seg_done.load(Relaxed);
    assert!(done > 0 && (done as usize) < SEGS, "暂停时已完成 {done} 段");
    m.save();
    let rec = store::load_tasks().into_iter().find(|r| r.id == t.id).unwrap();
    assert_eq!(rec.hls.as_ref().unwrap().done, done);
    m.start(t.id);
    wait(&t, Status::Done, 60);
    assert!(read(&t) == hls_expected("slow"), "续传后内容应一致");

    // 6. ffmpeg 合并（用脚本替身）：成功则得到 .mp4，失败则保留 .ts
    let ok_ff = e.dir.parent().unwrap().join("fake_ffmpeg.cmd");
    std::fs::write(&ok_ff, "@echo off\r\nset in=%7\r\nshift & shift & shift & shift & shift & shift & shift & shift & shift\r\ncopy /y /b %in% %3 >nul\r\n").unwrap();
    let bad_ff = e.dir.parent().unwrap().join("bad_ffmpeg.cmd");
    std::fs::write(&bad_ff, "@echo off\r\nexit /b 1\r\n").unwrap();
    let cur = m.settings.read().unwrap().clone();
    m.apply_settings(Settings { ffmpeg: ok_ff.display().to_string(), ..cur.clone() });
    let t = run("/hls/high/index.m3u8", "ff.ts");
    assert_eq!(t.status(), Status::Done, "{:?}", t.meta.lock().unwrap().error);
    assert!(t.final_path().extension().is_some_and(|x| x == "mp4"), "{:?}", t.final_path());
    assert!(read(&t) == hls_expected("high"));
    assert!(t.meta.lock().unwrap().hls.as_ref().unwrap().note.is_none());
    m.apply_settings(Settings { ffmpeg: bad_ff.display().to_string(), ..cur.clone() });
    let t = run("/hls/high/index.m3u8", "ffbad");
    assert!(t.final_path().extension().is_some_and(|x| x == "ts"), "{:?}", t.final_path());
    assert!(t.meta.lock().unwrap().hls.as_ref().unwrap().note.as_deref().unwrap().contains("转换 MP4 失败"));
    m.apply_settings(cur);

    // 7. 失败场景都要给出明确原因
    for (path, want) in [
        ("/hls/drm/index.m3u8", "DRM"),
        ("/hls/live/index.m3u8", "直播"),
        ("/hls/bad/index.m3u8", "404"),
        ("/hls/keybad/index.m3u8", "密钥"),
    ] {
        let t = run(path, "");
        assert_eq!(t.status(), Status::Error, "{path}");
        let err = t.meta.lock().unwrap().error.clone().unwrap();
        assert!(err.contains(want), "{path}: {err}");
    }
}

#[test]
fn reliability() {
    let e = env("reliable");
    let m = Manager::new(e.rt.handle().clone(), settings(&e));

    // 1. 超长路径（> 260 字符）下载、标记、删除
    let mut deep = e.dir.clone();
    for i in 0..8 {
        deep = deep.join(format!("很长的目录名称-long-directory-name-{i:02}"));
    }
    assert!(deep.as_os_str().len() > 300, "{}", deep.as_os_str().len());
    let id = m.add(NewTask {
        req: AddReq { url: format!("{}/file", e.base), referrer: Some("https://site.com/page".into()), ..Default::default() },
        name: "deep.bin".into(),
        dir: deep.clone(),
        auto_dir: false,
        conns: 8,
        mode: StartMode::Now,
        sha256: None,
    });
    let t = m.get(id).unwrap();
    wait(&t, Status::Done, 60);
    check(&t, &DATA);
    // 2. 下载来源标记
    let z = sys::read_mark(&t.final_path()).expect("应写入 Zone.Identifier");
    assert!(z.contains("ZoneId=3") && z.contains(&format!("HostUrl={}/file", e.base)) && z.contains("ReferrerUrl=https://site.com/page"), "{z}");
    m.remove(&[id], true);
    std::thread::sleep(Duration::from_millis(500));
    assert!(!t.final_path().exists(), "长路径文件应能删除");

    // 3. 磁盘空间不足：明确报错，且不自动重试
    sys::FAKE_FREE.store(1 << 20, Relaxed);
    let t = add(&m, &e, "/file", "nospace.bin", 8);
    wait(&t, Status::Error, 30);
    sys::FAKE_FREE.store(0, Relaxed);
    let err = t.meta.lock().unwrap().error.clone().unwrap();
    assert!(err.starts_with("磁盘空间不足"), "{err}");
    assert_eq!(t.meta.lock().unwrap().retries, u32::MAX);
    assert!(!t.meta.lock().unwrap().net_error);

    // 4. 睡眠唤醒：正在下载的任务重建连接（重新排队而不是暂停），最终完成
    let cur = m.settings.read().unwrap().clone();
    m.apply_settings(Settings { speed_limit_kb: 4096, ..cur.clone() });
    let t = add(&m, &e, "/file", "wake.bin", 4);
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(m.on_wake(), 1);
    std::thread::sleep(Duration::from_millis(300));
    assert_ne!(t.status(), Status::Paused, "唤醒后不应停在暂停状态");
    m.apply_settings(cur);
    wait(&t, Status::Done, 60);
    check(&t, &DATA);

    // 5. 网络类错误：标记为网络错误、不消耗重试次数，网络恢复时立即重新排队
    let id = m.add(NewTask {
        req: AddReq { url: "http://127.0.0.1:9/unreachable.bin".into(), ..Default::default() },
        name: "net.bin".into(),
        dir: e.dir.clone(),
        auto_dir: false,
        conns: 2,
        mode: StartMode::Now,
        sha256: None,
    });
    let t = m.get(id).unwrap();
    wait(&t, Status::Error, 30);
    assert!(t.meta.lock().unwrap().net_error, "{:?}", t.meta.lock().unwrap().error);
    assert_eq!(m.on_network_restored(), 1);
    assert_ne!(t.status(), Status::Error, "网络恢复后应重新排队");
    wait(&t, Status::Error, 30);
    assert_eq!(t.meta.lock().unwrap().retries, 0, "网络错误不消耗自动重试次数");
}

#[test]
fn sqlite_migration() {
    let root = std::env::temp_dir().join(format!("swiftdl-test-migrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    unsafe { std::env::set_var("APPDATA", root.join("appdata")) };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    // 构造旧版 tasks.json（含已完成与暂停任务）
    let mk = |id: u64, name: &str, status: Status| store::TaskRec {
        id,
        req: AddReq { url: format!("https://a.com/{name}"), ..Default::default() },
        name: name.into(),
        dir: root.clone(),
        conns: 8,
        size: Some(1000),
        resumable: true,
        validator: None,
        probed: true,
        status,
        error: None,
        created: 1,
        finished: 2,
        downloaded: 1000,
        segs: vec![],
        auto_dir: false,
        scheduled: false,
        sha256: None,
        hash: None,
        retries: 0,
        hls: None,
        bt: None,
    };
    let old = vec![mk(5, "a.zip", Status::Done), mk(9, "b.iso", Status::Paused), mk(7, "c.mp4", Status::Done)];
    store::write_bytes("tasks.json", &store::encode(&old));

    let m = Manager::new(rt.handle().clone(), Settings { dir: root.clone(), ..Default::default() });
    let names: Vec<String> = m.tasks.read().unwrap().iter().map(|t| t.meta.lock().unwrap().name.clone()).collect();
    assert_eq!(names, ["a.zip", "b.iso", "c.mp4"], "迁移后应保持顺序");
    assert!(!store::data_dir().join("tasks.json").exists());
    assert!(store::data_dir().join("tasks.json.bak").exists(), "旧文件应备份");
    assert_eq!(m.save(), 0, "迁移后立即保存不应有改动");
    drop(m);

    // 重新打开：从数据库读取，内容一致；删除只写一行
    let m2 = Manager::new(rt.handle().clone(), Settings { dir: root.clone(), ..Default::default() });
    assert_eq!(m2.tasks.read().unwrap().len(), 3);
    m2.remove(&[9], false);
    assert_eq!(m2.save(), 2, "删除中间的任务：删除一行 + 其后任务位置更新一行");
    drop(m2);
    assert_eq!(store::load_tasks().len(), 2);
    let _ = std::fs::remove_dir_all(&root);
}
