//! FTP / SFTP 端到端测试：测试内启动真实的 FTP 服务器（libunftp）与 SSH/SFTP 服务器（russh），
//! 验证多连接下载、续传、错误密码、文件不存在、主机密钥变化、密码不落盘。
use super::*;
use std::collections::HashMap;
use std::sync::LazyLock;

/// 仅用于测试的 SSH 主机密钥（ssh-keygen 生成，无实际用途）
const TEST_HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACCVs5bB5CvRwbT4YchWk/58MhBLVOV350ByDmBAqTSy6wAAAJhhL9C7YS/Q
uwAAAAtzc2gtZWQyNTUxOQAAACCVs5bB5CvRwbT4YchWk/58MhBLVOV350ByDmBAqTSy6w
AAAEDQHTrwEW2XwWyDWwjJf1f7eXWPRlvrpI45Ef1bCEFzsZWzlsHkK9HBtPhhyFaT/nwy
EEtU5XfnQHIOYECpNLLrAAAAEXN3aWZ0ZGwtdGVzdC1ob3N0AQIDBA==
-----END OPENSSH PRIVATE KEY-----
";

const LEN: usize = 8 << 20;
static FILE: LazyLock<Arc<Vec<u8>>> = LazyLock::new(|| {
    let mut x: u32 = 0x1234_5678;
    Arc::new(
        (0..LEN)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 24) as u8
            })
            .collect(),
    )
});

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn setup(name: &str) -> (tokio::runtime::Runtime, PathBuf, Arc<Manager>) {
    let root = std::env::temp_dir().join(format!("swiftdl-remote-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    unsafe { std::env::set_var("APPDATA", root.join("appdata")) };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let m = Manager::new(rt.handle().clone(), Settings { dir: root.join("dl"), proxy: "direct".into(), ..Default::default() });
    (rt, root, m)
}

fn add(m: &Arc<Manager>, url: String, name: &str, conns: u32, dir: &Path) -> Arc<Task> {
    let id = m.add(NewTask {
        req: AddReq { url, ..Default::default() },
        name: name.into(),
        dir: dir.to_path_buf(),
        auto_dir: false,
        conns,
        mode: StartMode::Now,
        sha256: None,
    });
    m.get(id).unwrap()
}

fn wait_end(t: &Task, secs: u64) -> Status {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let s = t.status();
        if matches!(s, Status::Done | Status::Error) {
            return s;
        }
        assert!(std::time::Instant::now() < deadline, "超时：{:?} {:?}", s, t.meta.lock().unwrap().error);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn ftp_download() {
    let (rt, root, m) = setup("ftp");
    let home = root.join("ftproot");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("data.bin"), &**FILE).unwrap();
    let port = free_port();
    let pasv = 30000 + (std::process::id() % 500) as u16 * 40;
    let h = home.clone();
    let server = libunftp::ServerBuilder::new(Box::new(move || unftp_sbe_fs::Filesystem::new(h.clone()).unwrap()))
        .passive_ports(pasv..=pasv + 39)
        .build()
        .unwrap();
    rt.spawn(server.listen(format!("127.0.0.1:{port}")));
    std::thread::sleep(Duration::from_millis(300));
    let dl = root.join("dl");

    // 探测：大小与续传支持
    let info = rt.block_on(remote::probe(&AddReq { url: format!("ftp://127.0.0.1:{port}/data.bin"), ..Default::default() })).unwrap();
    assert_eq!(info.size, Some(LEN as u64));
    assert!(info.resumable, "FTP 服务器支持 REST 时应可多连接续传");

    // 多连接下载，内容一致
    let t = add(&m, format!("ftp://127.0.0.1:{port}/data.bin"), "ftp.bin", 4, &dl);
    assert_eq!(wait_end(&t, 60), Status::Done, "{:?}", t.meta.lock().unwrap().error);
    assert!(std::fs::read(t.final_path()).unwrap() == **FILE, "FTP 下载内容应一致");

    // 暂停 → 续传
    let cur = m.settings.read().unwrap().clone();
    m.apply_settings(Settings { speed_limit_kb: 2048, ..cur.clone() });
    let t = add(&m, format!("ftp://127.0.0.1:{port}/data.bin"), "ftp-resume.bin", 4, &dl);
    std::thread::sleep(Duration::from_millis(1200));
    m.pause(t.id);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while t.status() != Status::Paused {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let mid = t.downloaded.load(Relaxed);
    assert!(mid > 0 && (mid as usize) < LEN, "暂停于 {mid}");
    m.apply_settings(cur);
    m.start(t.id);
    assert_eq!(wait_end(&t, 60), Status::Done, "{:?}", t.meta.lock().unwrap().error);
    assert!(std::fs::read(t.final_path()).unwrap() == **FILE, "FTP 续传后内容应一致");

    // 文件不存在：快速失败
    let t0 = std::time::Instant::now();
    let t = add(&m, format!("ftp://127.0.0.1:{port}/missing.bin"), "missing.bin", 4, &dl);
    assert_eq!(wait_end(&t, 30), Status::Error);
    assert!(t0.elapsed() < Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(&root);
}

// ───────────── SFTP 测试服务器 ─────────────

#[derive(Clone)]
struct SshSrv {
    channels: Arc<std::sync::Mutex<HashMap<russh::ChannelId, russh::Channel<russh::server::Msg>>>>,
}

impl russh::server::Handler for SshSrv {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<russh::server::Auth, Self::Error> {
        Ok(if user == "tester" && password == "s3cret" { russh::server::Auth::Accept } else { russh::server::Auth::reject() })
    }

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<russh::server::Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().unwrap().insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(&mut self, id: russh::ChannelId, name: &str, session: &mut russh::server::Session) -> Result<(), Self::Error> {
        let ch = self.channels.lock().unwrap().remove(&id);
        match ch {
            Some(ch) if name == "sftp" => {
                let _ = session.channel_success(id);
                tokio::spawn(russh_sftp::server::run(ch.into_stream(), SftpSrv));
            }
            _ => {
                let _ = session.channel_failure(id);
            }
        }
        Ok(())
    }
}

struct SftpSrv;

impl russh_sftp::server::Handler for SftpSrv {
    type Error = russh_sftp::protocol::StatusCode;

    fn unimplemented(&self) -> Self::Error {
        russh_sftp::protocol::StatusCode::OpUnsupported
    }

    async fn init(&mut self, _v: u32, _e: HashMap<String, String>) -> Result<russh_sftp::protocol::Version, Self::Error> {
        Ok(russh_sftp::protocol::Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        _f: russh_sftp::protocol::OpenFlags,
        _a: russh_sftp::protocol::FileAttributes,
    ) -> Result<russh_sftp::protocol::Handle, Self::Error> {
        if filename.ends_with("data.bin") {
            Ok(russh_sftp::protocol::Handle { id, handle: "h1".into() })
        } else {
            Err(russh_sftp::protocol::StatusCode::NoSuchFile)
        }
    }

    async fn read(&mut self, id: u32, _h: String, offset: u64, len: u32) -> Result<russh_sftp::protocol::Data, Self::Error> {
        let start = offset as usize;
        if start >= LEN {
            return Err(russh_sftp::protocol::StatusCode::Eof);
        }
        let end = (start + len as usize).min(LEN);
        Ok(russh_sftp::protocol::Data { id, data: FILE[start..end].to_vec() })
    }

    async fn close(&mut self, id: u32, _h: String) -> Result<russh_sftp::protocol::Status, Self::Error> {
        Ok(russh_sftp::protocol::Status {
            id,
            status_code: russh_sftp::protocol::StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        if !path.ends_with("data.bin") {
            return Err(russh_sftp::protocol::StatusCode::NoSuchFile);
        }
        let mut attrs = russh_sftp::protocol::FileAttributes::empty();
        attrs.size = Some(LEN as u64);
        attrs.mtime = Some(1_700_000_000);
        Ok(russh_sftp::protocol::Attrs { id, attrs })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn fstat(&mut self, id: u32, _h: String) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        self.stat(id, "data.bin".into()).await
    }
}

fn start_sftp(rt: &tokio::runtime::Runtime, key_pem: &str) -> u16 {
    let key = russh::keys::PrivateKey::from_openssh(key_pem).unwrap();
    let cfg = Arc::new(russh::server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::from_millis(10),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        ..Default::default()
    });
    let l = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = l.local_addr().unwrap().port();
    rt.spawn(async move {
        loop {
            let Ok((sock, _)) = l.accept().await else { break };
            let (cfg, h) = (cfg.clone(), SshSrv { channels: Default::default() });
            tokio::spawn(async move {
                if let Ok(s) = russh::server::run_stream(cfg, sock, h).await {
                    let _ = s.await;
                }
            });
        }
    });
    port
}

#[test]
fn sftp_download() {
    let (rt, root, m) = setup("sftp");
    let port = start_sftp(&rt, TEST_HOST_KEY);
    let dl = root.join("dl");
    let url = format!("sftp://tester:s3cret@127.0.0.1:{port}/data.bin");

    // 探测 + 首次连接记录主机密钥
    let info = rt.block_on(remote::probe(&AddReq { url: url.clone(), ..Default::default() })).unwrap();
    assert_eq!(info.size, Some(LEN as u64));
    let kh = std::fs::read_to_string(store::data_dir().join("known_hosts")).unwrap();
    assert!(kh.contains(&format!("127.0.0.1:{port} SHA256:")), "{kh}");

    // 多连接下载，内容一致；密码不以明文保存
    let t = add(&m, url.clone(), "sftp.bin", 4, &dl);
    assert!(!t.url().contains("s3cret"), "任务链接中不应保留密码");
    assert_eq!(wait_end(&t, 60), Status::Done, "{:?}", t.meta.lock().unwrap().error);
    assert!(std::fs::read(t.final_path()).unwrap() == **FILE, "SFTP 下载内容应一致");
    m.save();
    let rec = store::load_tasks().into_iter().find(|r| r.id == t.id).unwrap();
    let json = String::from_utf8(store::encode(&rec)).unwrap();
    assert!(!json.contains("s3cret") && rec.req.secret.is_some(), "保存的任务中密码应已加密");

    // 暂停 → 续传
    let cur = m.settings.read().unwrap().clone();
    m.apply_settings(Settings { speed_limit_kb: 2048, ..cur.clone() });
    let t = add(&m, url.clone(), "sftp-resume.bin", 4, &dl);
    std::thread::sleep(Duration::from_millis(1500));
    m.pause(t.id);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while t.status() != Status::Paused {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(t.downloaded.load(Relaxed) > 0);
    m.apply_settings(cur);
    m.start(t.id);
    assert_eq!(wait_end(&t, 60), Status::Done, "{:?}", t.meta.lock().unwrap().error);
    assert!(std::fs::read(t.final_path()).unwrap() == **FILE, "SFTP 续传后内容应一致");

    // 错误密码：快速失败并给出明确原因
    let t = add(&m, format!("sftp://tester:wrong@127.0.0.1:{port}/data.bin"), "bad.bin", 2, &dl);
    assert_eq!(wait_end(&t, 30), Status::Error);
    assert!(t.meta.lock().unwrap().error.clone().unwrap().contains("登录失败"));

    // 文件不存在
    let t = add(&m, format!("sftp://tester:s3cret@127.0.0.1:{port}/nope.bin"), "nope.bin", 2, &dl);
    assert_eq!(wait_end(&t, 30), Status::Error);

    // 主机密钥变化：把记录改成另一个指纹，连接应被拒绝
    std::fs::write(store::data_dir().join("known_hosts"), format!("127.0.0.1:{port} SHA256:someoneelse\n")).unwrap();
    let e = rt.block_on(remote::probe(&AddReq { url, ..Default::default() })).unwrap_err();
    assert!(e.contains("主机密钥已变化"), "{e}");
    let _ = std::fs::remove_dir_all(&root);
}

// ───────────── BT（离线：本进程内的做种方作为初始节点） ─────────────

#[test]
fn bt_download() {
    let (rt, root, m) = setup("bt");
    let cur = m.settings.read().unwrap().clone();
    m.apply_settings(Settings { bt_dht: false, bt_upnp: false, bt_port: 0, ..cur });

    // 做种方：包含两个文件的种子
    let seed_root = root.join("seed");
    let pack = seed_root.join("pack");
    std::fs::create_dir_all(&pack).unwrap();
    let a = &FILE[..3 << 20];
    let b = &FILE[3 << 20..5 << 20];
    std::fs::write(pack.join("a.bin"), a).unwrap();
    std::fs::write(pack.join("b.bin"), b).unwrap();
    let seeder = rt
        .block_on(librqbit::Session::new_with_opts(
            seed_root.clone(),
            librqbit::SessionOptions {
                dht: None,
                disable_trackers: true,
                listen: Some(librqbit::ListenerOptions {
                    listen_addr: "127.0.0.1:0".parse().unwrap(),
                    enable_upnp_port_forwarding: false,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))
        .unwrap();
    let (res, _h) = rt
        .block_on(seeder.create_and_serve_torrent(&pack, librqbit::CreateTorrentOptions { piece_length: Some(256 << 10), ..Default::default() }))
        .unwrap();
    let addr = seeder.listen_addr().unwrap();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", addr.port()).parse().unwrap();
    *bt::TEST_PEERS.lock().unwrap() = vec![addr];
    let torrent = root.join("pack.torrent");
    std::fs::write(&torrent, res.as_bytes().unwrap()).unwrap();
    let file_url = url::Url::from_file_path(&torrent).unwrap().to_string();
    std::thread::sleep(Duration::from_millis(500));

    // 1. 读取种子信息，只下载 b.bin
    let info = rt.block_on(bt::probe(&m, &m.client(), &AddReq { url: file_url.clone(), ..Default::default() })).unwrap();
    let bi = info.bt.unwrap();
    assert_eq!(bi.name, "pack");
    assert_eq!(bi.files.len(), 2);
    let idx_b = bi.files.iter().position(|f| f.0.ends_with("b.bin")).unwrap();
    let dl = root.join("dl");
    let id = m.add(NewTask {
        req: AddReq { url: file_url, bt_files: Some(vec![idx_b]), ..Default::default() },
        name: String::new(),
        dir: dl.clone(),
        auto_dir: false,
        conns: 8,
        mode: StartMode::Now,
        sha256: None,
    });
    let t = m.get(id).unwrap();
    assert_eq!(wait_end(&t, 90), Status::Done, "{:?}", t.meta.lock().unwrap().error);
    assert_eq!(t.meta.lock().unwrap().name, "pack");
    assert!(walk(&dl).iter().all(|f| f.starts_with(dl.join("pack"))), "多文件种子应放在以种子名命名的文件夹中：{:?}", walk(&dl));
    assert!(std::fs::read(dl.join("pack").join("b.bin")).unwrap() == b, "选中的文件内容应一致");
    assert!(std::fs::read(dl.join("pack").join("a.bin")).map(|d| d != a).unwrap_or(true), "未选中的文件不应完整下载");

    // 2. 磁力链接：从节点获取元数据后下载全部文件
    let dl2 = root.join("dl2");
    let id = m.add(NewTask {
        req: AddReq { url: res.as_magnet().to_string(), ..Default::default() },
        name: String::new(),
        dir: dl2.clone(),
        auto_dir: false,
        conns: 8,
        mode: StartMode::Now,
        sha256: None,
    });
    let t = m.get(id).unwrap();
    assert_eq!(wait_end(&t, 90), Status::Done, "{:?}", t.meta.lock().unwrap().error);
    assert!(std::fs::read(dl2.join("pack").join("a.bin")).unwrap() == a);
    assert!(std::fs::read(dl2.join("pack").join("b.bin")).unwrap() == b);
    assert_eq!(t.meta.lock().unwrap().size, Some(5 << 20));
    let hash = t.meta.lock().unwrap().bt.clone().unwrap().info_hash;
    assert_eq!(hash.len(), 40);

    bt::TEST_PEERS.lock().unwrap().clear();
    let _ = std::fs::remove_dir_all(&root);
}

fn walk(d: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(d) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { out.extend(walk(&p)); } else { out.push(p); }
        }
    }
    out
}
