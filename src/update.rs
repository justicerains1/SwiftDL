//! 自动更新：检查 GitHub 最新 Release → 下载 zip → 用发布资产的 SHA-256 校验 → 替换程序并重启。
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const REPO: &str = "justicerains1/SwiftDL";
pub const API: &str = "https://api.github.com";

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub page: String,
    pub zip_url: String,
    pub sha256: String,
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
        .header("user-agent", "SwiftDL-Updater")
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
        .ok_or(tr!("新版本缺少 SHA-256 校验值，为安全起见不自动更新"))?;
    Ok(Some(Release {
        version: r.tag_name.trim_start_matches('v').to_string(),
        notes: r.body.unwrap_or_default(),
        page: r.html_url,
        zip_url: asset.browser_download_url.clone(),
        sha256,
    }))
}

/// 下载并校验，解出新的 SwiftDL.exe，返回其路径
pub async fn download(client: &reqwest::Client, rel: &Release, work_dir: &Path) -> Result<PathBuf, String> {
    use sha2::Digest;
    let resp = client.get(&rel.zip_url).header("user-agent", "SwiftDL-Updater").send().await.map_err(crate::engine::probe::err_str)?;
    if !resp.status().is_success() {
        return Err(trf!("下载更新失败 (HTTP {})", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(crate::engine::probe::err_str)?;
    let got: String = sha2::Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if got != rel.sha256 {
        return Err(tr!("更新包校验失败（SHA-256 不一致），已取消更新").into());
    }
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| trf!("更新包损坏：{}", e))?;
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
    use axum::routing::get;
    use std::io::Write;

    #[test]
    fn versions() {
        assert!(is_newer("v0.5.0", "0.4.0"));
        assert!(is_newer("0.4.1", "0.4.0"));
        assert!(is_newer("v1.0", "0.9.9"));
        assert!(!is_newer("v0.4.0", "0.4.0"));
        assert!(!is_newer("v0.3.9", "0.4.0"));
        assert!(!is_newer("nightly", "0.4.0"));
    }

    fn make_zip(exe: &[u8]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opt = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            z.start_file("SwiftDL-v9.9.9-windows-x64/SwiftDL.exe", opt).unwrap();
            z.write_all(exe).unwrap();
            z.start_file("SwiftDL-v9.9.9-windows-x64/使用说明.txt", opt).unwrap();
            z.write_all("说明".as_bytes()).unwrap();
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn check_download_swap() {
        use sha2::Digest;
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let new_exe = b"MZ fake new swiftdl binary".repeat(1000);
        let zip_bytes = make_zip(&new_exe);
        let sha: String = sha2::Sha256::digest(&zip_bytes).iter().map(|b| format!("{b:02x}")).collect();
        let l = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let (zb, b2, sha2s) = (zip_bytes.clone(), base.clone(), sha.clone());
        let app = axum::Router::new()
            .route(
                "/repos/justicerains1/SwiftDL/releases/latest",
                get(move || {
                    let (b, s) = (b2.clone(), sha2s.clone());
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
            .route("/good.zip", get(move || async move { zb.clone() }));
        rt.spawn(async move { axum::serve(l, app).await.unwrap() });
        let c = reqwest::Client::builder().no_proxy().build().unwrap();

        let rel = rt.block_on(check(&c, &base)).unwrap().expect("应发现新版本");
        assert_eq!(rel.version, "9.9.9");
        assert_eq!(rel.sha256, sha);

        let dir = std::env::temp_dir().join(format!("swiftdl-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let got = rt.block_on(download(&c, &rel, &dir)).unwrap();
        assert_eq!(std::fs::read(&got).unwrap(), new_exe);

        // 校验值不符时拒绝
        let bad = Release { sha256: "00".repeat(32), ..rel.clone() };
        assert!(rt.block_on(download(&c, &bad, &dir)).unwrap_err().contains("校验失败"));

        // 替换：旧程序改名为 .old，新程序就位
        let target = dir.join("SwiftDL.exe");
        std::fs::write(&target, b"old version").unwrap();
        swap(&got, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), new_exe);
        assert_eq!(std::fs::read(target.with_extension("exe.old")).unwrap(), b"old version");
        // 新文件不存在时回滚
        std::fs::write(&target, b"current").unwrap();
        assert!(swap(&dir.join("missing.exe"), &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"current", "失败应回滚到原程序");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
