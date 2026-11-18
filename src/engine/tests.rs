//! 端到端测试：本地限速 Range 服务器 + 真实下载流程。
use super::*;
use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::get;
use futures_util::stream;
use std::sync::LazyLock;

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

/// 每连接约 8 MiB/s 的流式响应
fn slow_body(start: usize, end: usize) -> Body {
    let data = DATA.clone();
    let s = stream::unfold(start, move |pos| {
        let data = data.clone();
        async move {
            if pos >= end {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(8)).await;
            let e = (pos + 64 * 1024).min(end);
            Some((Ok::<_, std::io::Error>(data[pos..e].to_vec()), e))
        }
    });
    Body::from_stream(s)
}

async fn ranged(h: HeaderMap) -> Response<Body> {
    let range = h.get("range").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("bytes="));
    let Some(r) = range else {
        return Response::builder().header("content-length", SIZE).body(slow_body(0, SIZE)).unwrap();
    };
    let (a, b) = r.split_once('-').unwrap();
    let a: usize = a.parse().unwrap();
    let b: usize = if b.is_empty() { SIZE - 1 } else { b.parse::<usize>().unwrap().min(SIZE - 1) };
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header("content-range", format!("bytes {a}-{b}/{SIZE}"))
        .header("content-length", b - a + 1)
        .header("etag", "\"v1\"")
        .header("content-disposition", "attachment; filename*=UTF-8''%E6%B5%8B%E8%AF%95.bin")
        .body(slow_body(a, b + 1))
        .unwrap()
}

async fn norange() -> Response<Body> {
    Response::builder().header("content-length", SIZE).body(slow_body(0, SIZE)).unwrap()
}

async fn chunked() -> Response<Body> {
    Response::builder().body(slow_body(0, SIZE / 4)).unwrap()
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
    // 隔离配置目录（各测试独立进程变量不安全，所以测试串行在同一函数内执行）
    unsafe { std::env::set_var("APPDATA", root.join("appdata")) };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route("/file", get(ranged)).route("/norange", get(norange)).route("/chunked", get(chunked));
    rt.spawn(async move { axum::serve(listener, app).await.unwrap() });
    Env { rt, base, dir: root.join("dl") }
}

fn wait(t: &Task, want: Status, secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while t.status() != want {
        assert!(std::time::Instant::now() < deadline, "timeout waiting {want:?}, now {:?} err={:?}", t.status(), t.meta.lock().unwrap().error);
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn check(t: &Task, expect: &[u8]) {
    let got = std::fs::read(t.final_path()).unwrap();
    assert_eq!(got.len(), expect.len(), "length mismatch");
    assert!(got == expect, "content mismatch");
    assert!(!t.part_path().exists());
}

#[test]
fn end_to_end() {
    let e = env("e2e");
    let s = Settings { dir: e.dir.clone(), proxy: "direct".into(), max_active: 3, ..Default::default() };
    let m = Manager::new(e.rt.handle().clone(), s);
    let req = |p: &str| AddReq { url: format!("{}{p}", e.base), ..Default::default() };

    // 1. 多连接 + 动态分段 + Content-Disposition 中文名
    let t0 = std::time::Instant::now();
    let id = m.add(req("/file"), "", e.dir.clone(), 8, true);
    let t = m.get(id).unwrap();
    wait(&t, Status::Done, 60);
    let el = t0.elapsed();
    check(&t, &DATA);
    assert_eq!(t.meta.lock().unwrap().name, "测试.bin");
    println!("multi-conn 24MiB in {el:?}");

    // 2. 暂停 → 续传，内容必须一致
    let id = m.add(req("/file"), "resume.bin", e.dir.clone(), 4, true);
    let t = m.get(id).unwrap();
    std::thread::sleep(Duration::from_millis(600));
    m.pause(id);
    wait(&t, Status::Paused, 10);
    let mid = t.downloaded.load(Relaxed);
    assert!(mid > 0 && (mid as usize) < SIZE, "paused at {mid}");
    assert!(t.part_path().exists());
    // 模拟重启：从持久化记录恢复
    m.save();
    let rec = store::load_tasks().into_iter().find(|r| r.id == id).unwrap();
    assert_eq!(rec.status, Status::Paused);
    let restored = Task::from_rec(rec);
    assert_eq!(restored.downloaded.load(Relaxed), mid);
    m.start(id);
    wait(&t, Status::Done, 60);
    check(&t, &DATA);

    // 3. 不支持 Range：单连接
    let id = m.add(req("/norange"), "norange.bin", e.dir.clone(), 8, true);
    let t = m.get(id).unwrap();
    wait(&t, Status::Done, 60);
    check(&t, &DATA);
    assert!(!t.meta.lock().unwrap().resumable);

    // 4. 无 Content-Length
    let id = m.add(req("/chunked"), "chunked.bin", e.dir.clone(), 8, true);
    let t = m.get(id).unwrap();
    wait(&t, Status::Done, 60);
    check(&t, &DATA[..SIZE / 4]);

    // 5. 同名文件自动改名
    let id = m.add(req("/file"), "resume.bin", e.dir.clone(), 8, true);
    let t = m.get(id).unwrap();
    wait(&t, Status::Done, 60);
    assert_eq!(t.meta.lock().unwrap().name, "resume(1).bin");
    check(&t, &DATA);

    // 6. 限速：1 MiB/s 下载 2 秒应约 2 MiB
    let cur = m.settings.read().unwrap().clone();
    m.apply_settings(Settings { speed_limit_kb: 1024, ..cur });
    let id = m.add(req("/file"), "limited.bin", e.dir.clone(), 8, true);
    let t = m.get(id).unwrap();
    // 跳过起始突发（每连接已在 socket 缓冲中的首块数据），测稳态速率
    std::thread::sleep(Duration::from_millis(1500));
    let a = t.downloaded.load(Relaxed);
    std::thread::sleep(Duration::from_millis(3000));
    let b = t.downloaded.load(Relaxed);
    m.pause(id);
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
    m.remove(&[id], true);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!part.exists());
}

#[test]
fn queue_limit() {
    let e = env("queue");
    let s = Settings { dir: e.dir.clone(), proxy: "direct".into(), max_active: 2, ..Default::default() };
    let m = Manager::new(e.rt.handle().clone(), s);
    let ids: Vec<u64> = (0..5)
        .map(|i| m.add(AddReq { url: format!("{}/file", e.base), ..Default::default() }, &format!("q{i}.bin"), e.dir.clone(), 2, true))
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    let running = m.tasks.read().unwrap().iter().filter(|t| t.status() == Status::Running).count();
    assert_eq!(running, 2);
    for id in &ids {
        wait(&m.get(*id).unwrap(), Status::Done, 120);
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
    let id = m.add(AddReq { url, ..Default::default() }, "", root.clone(), 8, true);
    let t = m.get(id).unwrap();
    wait(&t, Status::Done, 600);
    let size = t.meta.lock().unwrap().size.unwrap();
    let el = t0.elapsed().as_secs_f64();
    println!("OK {} {} bytes in {el:.1}s ({:.1} MiB/s) segs-resumable={}", t.final_path().display(), size, size as f64 / el / 1048576.0, t.meta.lock().unwrap().resumable);
}
