//! 自动更新：检查 GitHub 最新 Release → 从多个下载源断点续传下载 zip → 用发布资产的 SHA-256 校验 → 替换程序并重启。
//!
//! 校验值来自严格校验证书的 GitHub API，因此安装包可以从任何镜像或加速代理下载：被篡改的包过不了校验。
use serde::Deserialize;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

pub const REPO: &str = "justicerains1/SwiftDL";
pub const API: &str = "https://api.github.com";
/// 下载加速代理：在原始地址前直接拼接（跳转后仍经过代理，支持 Range 续传）
pub const MIRRORS: &[&str] = &["https://down.avi.gs/"];

const UA: &str = "SwiftDL-Updater";
/// 探测下载源的超时
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// 连接或数据停滞超过该时间即换下一个下载源
const STALL: Duration = Duration::from_secs(15);
/// 低速检测：每个窗口内平均速度低于下限时换源（排在最后的源不做此检测，避免所有源都慢时反复放弃）
const SLOW_WINDOW: Duration = Duration::from_secs(20);
const SLOW_MIN_BPS: u64 = 16 * 1024;
/// 连续几轮所有下载源都毫无进展才放弃
const IDLE_ROUNDS: u32 = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub page: String,
    pub zip_url: String,
    pub sha256: String,
}

/// 下载进度（供界面显示）；`total` 为 0 表示大小未知
#[derive(Default, Debug)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

fn parse_ver(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let mut it = v.split(['.', '-', '+']).map(|x| x.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
}

pub fn is_newer(remote: &str, local: &str) -> bool {
    matches!((parse_ver(remote), parse_ver(local)), (Some(r), Some(l)) if r > l)
}

/// 检查是否有新版本；有则返回发布信息
pub async fn check(client: &reqwest::Client, api: &str) -> Result<Option<Release>, String> {
    let url = format!("{api}/repos/{REPO}/releases/latest");
    let resp = client
        .get(&url)
        .header("user-agent", UA)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(crate::engine::probe::err_str)?;
    match resp.status().as_u16() {
        200 => {}
        404 => return Ok(None), // 尚无正式发布
        s => return Err(trf!("检查更新失败 (HTTP {})", s)),
    }
    let body = resp.bytes().await.map_err(crate::engine::probe::err_str)?;
    let r: ApiRelease = serde_json::from_slice(&body).map_err(|e| trf!("解析发布信息失败：{}", e))?;
    if r.draft || r.prerelease || !is_newer(&r.tag_name, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let asset = r.assets.iter().find(|a| a.name.ends_with("-windows-x64.zip")).ok_or(tr!("新版本缺少 Windows 安装包"))?;
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase)
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or(tr!("新版本缺少 SHA-256 校验值，为安全起见不自动更新"))?;
    Ok(Some(Release {
        version: r.tag_name.trim_start_matches('v').to_string(),
        notes: r.body.unwrap_or_default(),
        page: r.html_url,
        zip_url: asset.browser_download_url.clone(),
        sha256,
    }))
}

/// 安装包的全部下载源：原始地址在前，其后是各加速代理
pub fn sources(url: &str) -> Vec<String> {
    std::iter::once(url.to_string()).chain(MIRRORS.iter().map(|m| format!("{m}{url}"))).collect()
}

/// 在浏览器中手动下载用的地址（优先加速代理）
pub fn browser_url(url: &str) -> String {
    sources(url).pop().unwrap_or_else(|| url.to_string())
}

/// 日志里只记录下载源的主机名
fn host_of(u: &str) -> String {
    crate::engine::host_key(u).unwrap_or_default()
}

/// 并发探测各下载源（只取 1 字节），按响应先后排序；探测失败的排在最后，仍作为兜底尝试
async fn rank(client: &reqwest::Client, srcs: &[String]) -> Vec<String> {
    let probes = srcs.iter().enumerate().map(|(i, u)| async move {
        let t = Instant::now();
        let req = client.get(u).header("user-agent", UA).header("range", "bytes=0-0").send();
        let ok = matches!(tokio::time::timeout(PROBE_TIMEOUT, req).await, Ok(Ok(r)) if r.status().is_success());
        if !ok {
            log_warn!("update source unreachable: {}", host_of(u));
        }
        (i, ok.then(|| t.elapsed()))
    });
    let mut r = futures_util::future::join_all(probes).await;
    r.sort_by_key(|&(i, d)| (d.is_none(), d, i));
    r.into_iter().map(|(i, _)| srcs[i].clone()).collect()
}

/// 解析 `Content-Range: bytes start-end/total`，返回 (start, total)；total 未知时为 0
fn content_range(v: &str) -> Option<(u64, u64)> {
    let (range, total) = v.trim().strip_prefix("bytes ")?.split_once('/')?;
    let start = range.split_once('-')?.0.trim().parse().ok()?;
    Some((start, total.trim().parse().unwrap_or(0)))
}

/// 从一个下载源续传到 `part` 文件；`offset` 随写入推进，失败时已写入的部分留给下一个源接着下
async fn fetch_from(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    offset: &mut u64,
    progress: &Progress,
    slow_check: bool,
) -> Result<(), String> {
    let mut req = client.get(url).header("user-agent", UA);
    if *offset > 0 {
        req = req.header("range", format!("bytes={}-", *offset));
    }
    let mut resp = tokio::time::timeout(STALL * 2, req.send())
        .await
        .map_err(|_| tr!("连接超时").to_string())?
        .map_err(crate::engine::probe::err_str)?;
    let total = match resp.status().as_u16() {
        206 => {
            let (start, total) = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(content_range)
                .ok_or(tr!("服务器返回的续传范围无效"))?;
            if start != *offset {
                return Err(tr!("服务器返回的续传范围无效").into());
            }
            total
        }
        200 => {
            *offset = 0; // 不支持续传：从头下载
            resp.content_length().unwrap_or(0)
        }
        416 if *offset > 0 => return Ok(()), // 已下载完整，交给校验判断
        s => return Err(trf!("下载更新失败 (HTTP {})", s)),
    };
    progress.total.store(total, Relaxed);
    let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(part).map_err(|e| e.to_string())?;
    f.set_len(*offset).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(*offset)).map_err(|e| e.to_string())?;
    progress.done.store(*offset, Relaxed);
    let mut win = (Instant::now(), *offset);
    loop {
        let chunk = match tokio::time::timeout(STALL, resp.chunk()).await {
            Err(_) => return Err(tr!("连接停滞").into()),
            Ok(r) => r.map_err(crate::engine::probe::err_str)?,
        };
        let Some(c) = chunk else { break };
        f.write_all(&c).map_err(|e| e.to_string())?;
        *offset += c.len() as u64;
        progress.done.store(*offset, Relaxed);
        if slow_check && win.0.elapsed() >= SLOW_WINDOW {
            let secs = win.0.elapsed().as_secs().max(1);
            if (*offset - win.1) / secs < SLOW_MIN_BPS {
                return Err(tr!("速度过慢").into());
            }
            win = (Instant::now(), *offset);
        }
    }
    if total > 0 && *offset < total {
        return Err(tr!("连接中断").into());
    }
    Ok(())
}

/// 下载并校验，解出新的 SwiftDL.exe，返回其路径。
///
/// 先探测各下载源并按响应快慢排序，依次尝试；某个源失败、停滞或过慢时换下一个，从已下载的位置续传。
/// 未完成的下载保存在 `work_dir`，下次点击更新会接着下载。
pub async fn download(
    client: &reqwest::Client,
    rel: &Release,
    srcs: &[String],
    work_dir: &Path,
    progress: &Progress,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(work_dir).map_err(|e| e.to_string())?;
    let name = format!("update-{}.zip.part", rel.sha256.get(..16).unwrap_or(&rel.sha256));
    let part = work_dir.join(&name);
    // 清理其他版本遗留的未完成下载
    if let Ok(rd) = std::fs::read_dir(work_dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("update-") && n.ends_with(".zip.part") && n != name {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let mut offset = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let order = rank(client, srcs).await;
    let mut last_err = String::new();
    let mut idle = 0;
    let mut done = false;
    'rounds: while idle < IDLE_ROUNDS {
        let before = offset;
        for (i, url) in order.iter().enumerate() {
            let slow_check = i + 1 < order.len();
            match fetch_from(client, url, &part, &mut offset, progress, slow_check).await {
                Ok(()) => {
                    log_info!("update downloaded from {}", host_of(url));
                    done = true;
                    break 'rounds;
                }
                Err(e) => {
                    log_warn!("update source {} failed at {offset} bytes: {e}", host_of(url));
                    last_err = e;
                }
            }
        }
        if offset == before {
            idle += 1;
            if idle < IDLE_ROUNDS {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    if !done {
        return Err(trf!("所有下载源均失败（{}）", last_err));
    }
    // 无论成败都删除：成功则已解出程序；失败说明内容有误，下次应从头下载
    let r = install_zip(rel, &part, work_dir);
    let _ = std::fs::remove_file(&part);
    r
}

/// 校验本地安装包（自动下载的或用户手动下载的）并解出新的 SwiftDL.exe
pub fn install_zip(rel: &Release, zip_path: &Path, work_dir: &Path) -> Result<PathBuf, String> {
    let got = crate::engine::sha256_file(zip_path).map_err(|e| e.to_string())?;
    if got != rel.sha256 {
        return Err(tr!("更新包校验失败（SHA-256 不一致），已取消更新").into());
    }
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| trf!("更新包损坏：{}", e))?;
    let idx = (0..zip.len())
        .find(|&i| {
            zip.by_index(i)
                .ok()
                .and_then(|f| f.name().ok().map(|n| n.replace('\\', "/")))
                .is_some_and(|n| n.rsplit('/').next() == Some("SwiftDL.exe"))
        })
        .ok_or(tr!("更新包中没有 SwiftDL.exe"))?;
    std::fs::create_dir_all(work_dir).map_err(|e| e.to_string())?;
    let out = work_dir.join("SwiftDL.new.exe");
    let mut f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
    std::io::copy(&mut zip.by_index(idx).map_err(|e| e.to_string())?, &mut f).map_err(|e| e.to_string())?;
    Ok(out)
}

/// 用新程序替换当前程序：运行中的 exe 可以改名但不能覆盖，因此先改名为 .old 再放入新文件；失败则回滚
pub fn swap(new_exe: &Path, target: &Path) -> Result<(), String> {
    let old = target.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(target, &old).map_err(|e| trf!("无法替换程序（{}），请手动下载新版本", e))?;
    if let Err(e) = std::fs::copy(new_exe, target) {
        let _ = std::fs::rename(&old, target);
        return Err(trf!("写入新程序失败（{}），已恢复原版本", e));
    }
    let _ = std::fs::remove_file(new_exe);
    Ok(())
}

/// 启动时清理上次更新留下的旧程序
pub fn cleanup_old() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }
}

/// 替换并重新启动
pub fn apply_and_restart(new_exe: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    swap(new_exe, &exe)?;
    std::process::Command::new(&exe).arg("--updated").spawn().map_err(|e| trf!("新版本启动失败：{}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::routing::get;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn versions() {
        assert!(is_newer("v0.5.0", "0.4.0"));
        assert!(is_newer("0.4.1", "0.4.0"));
        assert!(is_newer("v1.0", "0.9.9"));
        assert!(!is_newer("v0.4.0", "0.4.0"));
        assert!(!is_newer("v0.3.9", "0.4.0"));
        assert!(!is_newer("nightly", "0.4.0"));
    }

    #[test]
    fn source_list() {
        let u = "https://github.com/o/r/releases/download/v1/a.zip";
        assert_eq!(sources(u), vec![u.to_string(), format!("https://down.avi.gs/{u}")]);
        assert_eq!(browser_url(u), format!("https://down.avi.gs/{u}"));
        assert_eq!(content_range("bytes 100-199/1000"), Some((100, 1000)));
        assert_eq!(content_range("bytes 5-9/*"), Some((5, 0)));
        assert_eq!(content_range("items 1-2/3"), None);
    }

    fn make_zip(exe: &[u8]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opt = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            z.start_file("SwiftDL-v9.9.9-windows-x64/SwiftDL.exe", opt).unwrap();
            z.write_all(exe).unwrap();
            z.start_file("SwiftDL-v9.9.9-windows-x64/使用说明.txt", opt).unwrap();
            z.write_all("说明".as_bytes()).unwrap();
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    fn sha_hex(b: &[u8]) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(b).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 支持 Range 的下载响应；记录收到的续传起点
    fn ranged(data: &[u8], h: &HeaderMap, seen: &std::sync::Mutex<Vec<u64>>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let start = h
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.split('-').next())
            .and_then(|v| v.parse::<usize>().ok());
        let mut hm = HeaderMap::new();
        match start {
            Some(s) if s >= data.len() => (StatusCode::RANGE_NOT_SATISFIABLE, hm, vec![]),
            Some(s) => {
                seen.lock().unwrap().push(s as u64);
                let end = if h.get(header::RANGE).unwrap().to_str().unwrap().ends_with("-0") { s + 1 } else { data.len() };
                hm.insert(header::CONTENT_RANGE, format!("bytes {s}-{}/{}", end - 1, data.len()).parse().unwrap());
                (StatusCode::PARTIAL_CONTENT, hm, data[s..end].to_vec())
            }
            None => (StatusCode::OK, hm, data.to_vec()),
        }
    }

    struct Srv {
        rt: tokio::runtime::Runtime,
        base: String,
        zip: Vec<u8>,
        exe: Vec<u8>,
        /// /ranged.zip 收到的续传起点（不含探测请求的 0）
        seen: Arc<std::sync::Mutex<Vec<u64>>>,
        /// /blocked.zip 被请求的次数
        blocked_hits: Arc<AtomicUsize>,
    }

    fn server() -> Srv {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let exe = b"MZ fake new swiftdl binary".repeat(4000);
        let zip = make_zip(&exe);
        let sha = sha_hex(&zip);
        let l = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let blocked_hits = Arc::new(AtomicUsize::new(0));
        let (b2, s2, z1, z2, z3, seen2, bh) = (base.clone(), sha.clone(), zip.clone(), zip.clone(), zip.clone(), seen.clone(), blocked_hits.clone());
        let app = axum::Router::new()
            .route(
                "/repos/justicerains1/SwiftDL/releases/latest",
                get(move || {
                    let (b, s) = (b2.clone(), s2.clone());
                    async move {
                        axum::Json(serde_json::json!({
                            "tag_name": "v9.9.9", "body": "更新说明", "html_url": "https://github.com/x",
                            "assets": [
                                {"name": "SwiftDL-v9.9.9-windows-x64.zip", "browser_download_url": format!("{b}/good.zip"), "digest": format!("sha256:{s}")},
                            ]
                        }))
                    }
                }),
            )
            // 不支持 Range：总是返回整个文件
            .route("/good.zip", get(move || async move { z1.clone() }))
            // 支持 Range
            .route(
                "/ranged.zip",
                get(move |h: HeaderMap| {
                    let (z, s) = (z2.clone(), seen2.clone());
                    async move {
                        let r = ranged(&z, &h, &s);
                        s.lock().unwrap().retain(|&x| x != 0);
                        r
                    }
                }),
            )
            // 模拟被墙的源
            .route(
                "/blocked.zip",
                get(move || {
                    bh.fetch_add(1, Relaxed);
                    async { StatusCode::FORBIDDEN }
                }),
            )
            // 内容被篡改的源
            .route(
                "/evil.zip",
                get(move || {
                    let mut z = z3.clone();
                    z[100] ^= 0xff;
                    async move { z }
                }),
            );
        rt.spawn(async move { axum::serve(l, app).await.unwrap() });
        Srv { rt, base, zip, exe, seen, blocked_hits }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("swiftdl-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[test]
    fn check_download_swap() {
        let s = server();
        let c = client();
        let rel = s.rt.block_on(check(&c, &s.base)).unwrap().expect("应发现新版本");
        assert_eq!(rel.version, "9.9.9");
        assert_eq!(rel.sha256, sha_hex(&s.zip));

        let dir = tmp("basic");
        let p = Progress::default();
        let got = s.rt.block_on(download(&c, &rel, std::slice::from_ref(&rel.zip_url), &dir, &p)).unwrap();
        assert_eq!(std::fs::read(&got).unwrap(), s.exe);
        assert_eq!(p.done.load(Relaxed), s.zip.len() as u64);
        assert!(!std::fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().ends_with(".part")), "完成后应删除临时文件");

        // 校验值不符时拒绝
        let bad = Release { sha256: "00".repeat(32), ..rel.clone() };
        assert!(s.rt.block_on(download(&c, &bad, std::slice::from_ref(&rel.zip_url), &dir, &p)).unwrap_err().contains("校验失败"));

        // 替换：旧程序改名为 .old，新程序就位
        let target = dir.join("SwiftDL.exe");
        std::fs::write(&target, b"old version").unwrap();
        swap(&got, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), s.exe);
        assert_eq!(std::fs::read(target.with_extension("exe.old")).unwrap(), b"old version");
        // 新文件不存在时回滚
        std::fs::write(&target, b"current").unwrap();
        assert!(swap(&dir.join("missing.exe"), &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"current", "失败应回滚到原程序");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn rel_for(s: &Srv) -> Release {
        Release {
            version: "9.9.9".into(),
            notes: String::new(),
            page: String::new(),
            zip_url: format!("{}/good.zip", s.base),
            sha256: sha_hex(&s.zip),
        }
    }

    #[test]
    fn falls_back_to_next_source() {
        let s = server();
        let rel = rel_for(&s);
        let dir = tmp("fallback");
        let srcs = [format!("{}/blocked.zip", s.base), format!("{}/ranged.zip", s.base)];
        let got = s.rt.block_on(download(&client(), &rel, &srcs, &dir, &Progress::default())).unwrap();
        assert_eq!(std::fs::read(&got).unwrap(), s.exe);
        // 被墙的源探测失败后排到最后，可用的源成功后不再请求它
        assert_eq!(s.blocked_hits.load(Relaxed), 1, "只应被探测一次");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resumes_partial_download() {
        let s = server();
        let rel = rel_for(&s);
        let dir = tmp("resume");
        std::fs::create_dir_all(&dir).unwrap();
        let half = s.zip.len() / 2;
        let part = dir.join(format!("update-{}.zip.part", &rel.sha256[..16]));
        std::fs::write(&part, &s.zip[..half]).unwrap();
        // 其他版本遗留的临时文件应被清理
        let stale = dir.join("update-0000000000000000.zip.part");
        std::fs::write(&stale, b"stale").unwrap();
        let got = s.rt.block_on(download(&client(), &rel, &[format!("{}/ranged.zip", s.base)], &dir, &Progress::default())).unwrap();
        assert_eq!(std::fs::read(&got).unwrap(), s.exe);
        assert_eq!(*s.seen.lock().unwrap(), vec![half as u64], "应从已下载的位置续传");
        assert!(!stale.exists());

        // 服务器不支持 Range 时从头下载，残留内容不影响结果
        std::fs::write(&part, b"garbage garbage").unwrap();
        let got = s.rt.block_on(download(&client(), &rel, std::slice::from_ref(&rel.zip_url), &dir, &Progress::default())).unwrap();
        assert_eq!(std::fs::read(&got).unwrap(), s.exe);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_source_rejected_and_local_install() {
        let s = server();
        let rel = rel_for(&s);
        let dir = tmp("tamper");
        let e = s.rt.block_on(download(&client(), &rel, &[format!("{}/evil.zip", s.base)], &dir, &Progress::default())).unwrap_err();
        assert!(e.contains("校验失败"), "{e}");
        assert!(!dir.join(format!("update-{}.zip.part", &rel.sha256[..16])).exists(), "篡改的内容不应留作续传");

        // 用户手动下载的安装包：校验通过才解出
        let zip = dir.join("SwiftDL-v9.9.9-windows-x64.zip");
        std::fs::write(&zip, &s.zip).unwrap();
        let got = install_zip(&rel, &zip, &dir).unwrap();
        assert_eq!(std::fs::read(&got).unwrap(), s.exe);
        std::fs::write(&zip, &s.zip[..s.zip.len() - 1]).unwrap();
        assert!(install_zip(&rel, &zip, &dir).unwrap_err().contains("校验失败"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
