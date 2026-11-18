//! BT / 磁力：基于 librqbit。磁力链接、.torrent 链接或本地文件 → 获取元数据 → 选择文件 → 下载（可做种）。
use super::probe::Info;
use super::{AddReq, Manager, Task, store};
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Session, SessionOptions};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering::*;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const CANCELLED: &str = "cancelled";
/// 磁力链接获取元数据的最长等待时间
const META_TIMEOUT: Duration = Duration::from_secs(180);

static SESSION: tokio::sync::OnceCell<Arc<Session>> = tokio::sync::OnceCell::const_new();

/// 测试用：额外的初始节点（离线测试时直接连接本地做种方）
#[cfg(test)]
pub static TEST_PEERS: std::sync::Mutex<Vec<SocketAddr>> = std::sync::Mutex::new(Vec::new());

/// 持久化的 BT 任务信息
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BtState {
    pub info_hash: String,
    /// 选中下载的文件序号；None 表示全部
    pub files: Option<Vec<usize>>,
    pub file_count: usize,
    pub uploaded: u64,
    pub peers: u32,
}

/// 新建对话框展示的种子信息
#[derive(Clone, Debug)]
pub struct BtInfo {
    pub name: String,
    pub info_hash: String,
    pub files: Vec<(String, u64)>,
}

pub fn is_bt(url: &str) -> bool {
    let l = url.trim().to_ascii_lowercase();
    l.starts_with("magnet:?") || (l.split(['?', '#']).next().unwrap_or("").ends_with(".torrent") && (l.starts_with("http") || l.starts_with("file:")))
}

fn torrent_dir() -> PathBuf {
    store::data_dir().join("bt")
}

fn limits(m: &Manager) -> (Option<NonZeroU32>, Option<NonZeroU32>) {
    let s = m.settings.read().unwrap();
    let kb = |v: u64| NonZeroU32::new((v * 1024).min(u32::MAX as u64) as u32);
    (kb(s.speed_limit_kb), kb(s.bt_upload_kb))
}

/// 全局 BT 会话（首次使用时启动；端口、DHT、UPnP 设置在重启后生效）
pub async fn session(m: &Manager) -> Result<Arc<Session>, String> {
    let s = SESSION
        .get_or_try_init(|| async {
            let st = m.settings.read().unwrap().clone();
            let (down, up) = limits(m);
            let opts = SessionOptions {
                dht: st.bt_dht.then(librqbit::DhtSessionConfig::default),
                listen: Some(librqbit::ListenerOptions {
                    listen_addr: SocketAddr::from(([0, 0, 0, 0], st.bt_port)),
                    enable_upnp_port_forwarding: st.bt_upnp,
                    ..Default::default()
                }),
                disable_trackers: !st.bt_dht,
                fastresume: false,
                persistence: None,
                ratelimits: librqbit::limits::LimitsConfig { download_bps: down, upload_bps: up },
                ..Default::default()
            };
            Session::new_with_opts(st.dir.clone(), opts).await.map_err(|e| trf!("BT 引擎启动失败：{}", e))
        })
        .await?;
    Ok(s.clone())
}

/// 设置变化时同步限速
pub fn apply_limits(m: &Manager) {
    if let Some(s) = SESSION.get() {
        let (down, up) = limits(m);
        s.ratelimits.set_download_bps(down);
        s.ratelimits.set_upload_bps(up);
    }
}

/// 读取种子来源：磁力链接直接交给引擎；.torrent 从网络或本地读取
async fn source(client: &reqwest::Client, req: &AddReq) -> Result<AddTorrent<'static>, String> {
    let url = req.url.trim().to_string();
    if url.to_ascii_lowercase().starts_with("magnet:") {
        return Ok(AddTorrent::from_url(url));
    }
    if url.to_ascii_lowercase().starts_with("file:") {
        let path = url::Url::parse(&url).ok().and_then(|u| u.to_file_path().ok()).ok_or_else(|| trf!("无效的链接：{}", url))?;
        let bytes = std::fs::read(&path).map_err(|e| trf!("读取种子文件失败：{}", e))?;
        return Ok(AddTorrent::from_bytes(bytes));
    }
    let resp = super::probe::with_headers(client.get(&url), req).send().await.map_err(super::probe::err_str)?;
    if !resp.status().is_success() {
        return Err(trf!("下载种子文件失败 (HTTP {})", resp.status().as_u16()));
    }
    Ok(AddTorrent::from_bytes(resp.bytes().await.map_err(super::probe::err_str)?.to_vec()))
}

fn test_peers() -> Option<Vec<SocketAddr>> {
    #[cfg(test)]
    {
        let p = TEST_PEERS.lock().unwrap().clone();
        if !p.is_empty() {
            return Some(p);
        }
    }
    None
}

/// 获取种子元数据（文件列表），并缓存种子文件供后续续传
pub async fn probe(m: &Manager, client: &reqwest::Client, req: &AddReq) -> Result<Info, String> {
    let sess = session(m).await?;
    let src = source(client, req).await?;
    let opts = AddTorrentOptions { list_only: true, initial_peers: test_peers(), ..Default::default() };
    let r = tokio::time::timeout(META_TIMEOUT, sess.add_torrent(src, Some(opts)))
        .await
        .map_err(|_| tr!("获取种子信息超时：没有找到可用的节点").to_string())?
        .map_err(|e| trf!("获取种子信息失败：{}", e))?;
    let AddTorrentResponse::ListOnly(r) = r else {
        return Err(tr!("获取种子信息失败").into());
    };
    let hash = r.info_hash.as_string();
    let _ = std::fs::create_dir_all(torrent_dir());
    let _ = std::fs::write(torrent_dir().join(format!("{hash}.torrent")), &r.torrent_bytes);
    let files: Vec<(String, u64)> = r.info.iter_file_details().map(|f| (f.filename.to_string(), f.len)).collect();
    let name = super::probe::sanitize(&r.info.name().map(|n| n.to_string()).unwrap_or_else(|| hash.clone()));
    let total = files.iter().map(|f| f.1).sum();
    Ok(Info {
        size: Some(total),
        resumable: true,
        filename: name.clone(),
        validator: Some(hash.clone()),
        hls: None,
        bt: Some(BtInfo { name, info_hash: hash, files }),
    })
}

/// 执行 BT 任务：加入（或恢复）会话中的种子，同步进度直到完成或被取消
pub async fn run(m: &Arc<Manager>, t: &Arc<Task>, tok: &CancellationToken, client: reqwest::Client) -> Result<(), String> {
    let sess = session(m).await?;
    let req = t.req();
    let (dir, prev) = {
        let meta = t.meta.lock().unwrap();
        (meta.dir.clone(), meta.bt.clone().unwrap_or_default())
    };
    std::fs::create_dir_all(&dir).map_err(|e| trf!("无法创建目录: {}", e))?;
    // 先取得元数据：已缓存的种子文件立即可用；磁力链接需从节点获取（之后用得到的种子数据正式添加）
    let cached = torrent_dir().join(format!("{}.torrent", prev.info_hash));
    let src = if !prev.info_hash.is_empty() && cached.exists() {
        AddTorrent::from_bytes(std::fs::read(&cached).map_err(|e| e.to_string())?)
    } else {
        tokio::select! {
            r = source(&client, &req) => r?,
            _ = tok.cancelled() => return Err(CANCELLED.into()),
        }
    };
    let only = prev.files.clone().or(req.bt_files.clone());
    let meta_opts = AddTorrentOptions { list_only: true, initial_peers: test_peers(), ..Default::default() };
    let listed = tokio::select! {
        r = tokio::time::timeout(META_TIMEOUT, sess.add_torrent(src, Some(meta_opts))) => r
            .map_err(|_| tr!("获取种子信息超时：没有找到可用的节点").to_string())?
            .map_err(|e| trf!("添加 BT 任务失败：{}", e))?,
        _ = tok.cancelled() => return Err(CANCELLED.into()),
    };
    let resp = match listed {
        // 会话中已有（暂停后继续）：直接沿用
        AddTorrentResponse::AlreadyManaged(id, h) => AddTorrentResponse::AlreadyManaged(id, h),
        AddTorrentResponse::Added(id, h) => AddTorrentResponse::Added(id, h),
        AddTorrentResponse::ListOnly(r) => {
            let hash = r.info_hash.as_string();
            let _ = std::fs::create_dir_all(torrent_dir());
            let _ = std::fs::write(torrent_dir().join(format!("{hash}.torrent")), &r.torrent_bytes);
            let name = super::probe::sanitize(&r.info.name().map(|n| n.to_string()).unwrap_or_else(|| hash.clone()));
            let multi = r.info.iter_file_details().count() > 1;
            // 多文件种子放进以种子名命名的文件夹
            let out = if multi { dir.join(&name) } else { dir.clone() };
            {
                let mut meta = t.meta.lock().unwrap();
                meta.name = name;
                let mut st = meta.bt.clone().unwrap_or_default();
                st.info_hash = hash;
                meta.bt = Some(st);
            }
            let opts = AddTorrentOptions {
                only_files: only.clone(),
                output_folder: Some(out.display().to_string()),
                overwrite: true,
                initial_peers: test_peers(),
                ..Default::default()
            };
            tokio::select! {
                r = sess.add_torrent(AddTorrent::from_bytes(r.torrent_bytes.to_vec()), Some(opts)) => r.map_err(|e| trf!("添加 BT 任务失败：{}", e))?,
                _ = tok.cancelled() => return Err(CANCELLED.into()),
            }
        }
    };
    let (id, handle) = match resp {
        AddTorrentResponse::Added(id, h) | AddTorrentResponse::AlreadyManaged(id, h) => (id, h),
        AddTorrentResponse::ListOnly(_) => return Err(tr!("添加 BT 任务失败").into()),
    };
    if handle.is_paused() {
        sess.unpause(&handle).await.map_err(|e| e.to_string())?;
    }
    if let Some(files) = &only {
        let set: std::collections::HashSet<usize> = files.iter().copied().collect();
        let _ = sess.update_only_files(&handle, &set).await;
    }
    let hash = handle.info_hash().as_string();
    {
        let mut meta = t.meta.lock().unwrap();
        if meta.name.is_empty()
            && let Some(n) = handle.name()
        {
            meta.name = super::probe::sanitize(&n);
        }
        let mut st = meta.bt.clone().unwrap_or_default();
        st.info_hash = hash.clone();
        st.files = only;
        meta.bt = Some(st);
    }
    m.mark_dirty();
    m.repaint();

    let seed = m.settings.read().unwrap().bt_seed;
    loop {
        tokio::select! {
            _ = tok.cancelled() => {
                let _ = sess.pause(&handle).await;
                return Err(CANCELLED.into());
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        let s = handle.stats();
        t.downloaded.store(s.progress_bytes, Relaxed);
        let peers = s.live.as_ref().map(|l| l.snapshot.peer_stats.live).unwrap_or(0);
        t.active_conns.store(peers, Relaxed);
        {
            let mut meta = t.meta.lock().unwrap();
            if s.total_bytes > 0 {
                meta.size = Some(s.total_bytes);
            }
            if let Some(st) = meta.bt.as_mut() {
                st.uploaded = s.uploaded_bytes;
                st.peers = peers;
                st.file_count = s.file_progress.len();
            }
        }
        if let Some(e) = s.error {
            return Err(trf!("BT 下载出错：{}", e));
        }
        if s.finished {
            if !seed {
                let _ = sess.delete(id.into(), false).await;
            }
            return Ok(());
        }
    }
}

/// 删除任务时从会话移除种子（可同时删除已下载的文件）
pub async fn forget(info_hash: &str, delete_files: bool) {
    let Some(sess) = SESSION.get() else { return };
    if let Ok(id) = info_hash.parse::<librqbit::dht::Id20>() {
        let _ = sess.delete(id.into(), delete_files).await;
    }
}

/// 关联磁力链接与 .torrent 文件到本程序（当前用户，可撤销）
pub fn set_association(on: bool) -> bool {
    use std::os::windows::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else { return false };
    let cmd = format!("\"{}\" \"%1\"", exe.display());
    let run = |args: &[&str]| {
        std::process::Command::new("reg").args(args).creation_flags(0x0800_0000).status().is_ok_and(|s| s.success())
    };
    let k = r"HKCU\Software\Classes";
    if on {
        run(&["add", &format!(r"{k}\magnet"), "/ve", "/d", "URL:Magnet Protocol", "/f"])
            && run(&["add", &format!(r"{k}\magnet"), "/v", "URL Protocol", "/d", "", "/f"])
            && run(&["add", &format!(r"{k}\magnet\shell\open\command"), "/ve", "/d", &cmd, "/f"])
            && run(&["add", &format!(r"{k}\.torrent"), "/ve", "/d", "SwiftDL.Torrent", "/f"])
            && run(&["add", &format!(r"{k}\SwiftDL.Torrent\shell\open\command"), "/ve", "/d", &cmd, "/f"])
    } else {
        let a = run(&["delete", &format!(r"{k}\magnet"), "/f"]);
        let b = run(&["delete", &format!(r"{k}\SwiftDL.Torrent"), "/f"]);
        a || b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection() {
        assert!(is_bt("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567"));
        assert!(is_bt("https://site.com/files/ubuntu.iso.torrent?dl=1"));
        assert!(is_bt("file:///C:/Users/a/x.torrent"));
        assert!(!is_bt("https://site.com/x.iso"));
        assert!(!is_bt("ftp://h/x.torrent"));
    }
}
