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
        .route("/chunked", get(chunked));
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

    // 9. 内容未变化时不重复写盘
    m.save();
    let tasks_json = store::data_dir().join("tasks.json");
    std::fs::remove_file(&tasks_json).unwrap();
    m.save();
    assert!(!tasks_json.exists(), "内容未变化不应写盘");
    m.mark_dirty();
    let second = m.tasks.read().unwrap()[1].id;
    m.move_task(second, Move::Top);
    m.save();
    assert!(tasks_json.exists(), "内容变化后应写盘");
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
