//! 浏览器扩展安装助手：扩展文件内嵌在 exe 中，释放到数据目录后引导用户一键加载。
//! Chrome/Edge 不允许程序静默安装商店之外的扩展，因此采用“打开扩展页 + 定位文件夹”的引导方式。
use crate::engine::store;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const FILES: [(&str, &[u8]); 6] = [
    ("manifest.json", include_bytes!("../extension/manifest.json")),
    ("background.js", include_bytes!("../extension/background.js")),
    ("sniff.js", include_bytes!("../extension/sniff.js")),
    ("content.js", include_bytes!("../extension/content.js")),
    ("popup.html", include_bytes!("../extension/popup.html")),
    ("popup.js", include_bytes!("../extension/popup.js")),
];

/// 扩展最近一次连接本程序的时间（秒），0 = 本次运行尚未连接
pub static LAST_HELLO: AtomicU64 = AtomicU64::new(0);

pub fn connected() -> bool {
    LAST_HELLO.load(Ordering::Relaxed) != 0
}

pub fn ext_dir() -> PathBuf {
    store::data_dir().join("extension")
}

/// 释放扩展文件（内容相同则不重写，避免浏览器认为扩展被修改），并写入配对信息 config.json
pub fn extract(token: &str, port: u16) -> std::io::Result<PathBuf> {
    let dir = ext_dir();
    std::fs::create_dir_all(&dir)?;
    let config = format!("{{\"token\":\"{token}\",\"port\":{port}}}");
    for (name, data) in FILES.iter().copied().chain(std::iter::once(("config.json", config.as_bytes()))) {
        let p = dir.join(name);
        if std::fs::read(&p).ok().as_deref() != Some(data) {
            std::fs::write(&p, data)?;
        }
    }
    Ok(dir)
}

/// 扩展文件夹已释放过时，更新其中的配对信息（重新生成令牌后调用）
pub fn refresh_config(token: &str, port: u16) {
    if ext_dir().join("manifest.json").exists() {
        let _ = extract(token, port);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Browser {
    Edge,
    Chrome,
}

impl Browser {
    pub fn name(self) -> &'static str {
        match self {
            Self::Edge => "Microsoft Edge",
            Self::Chrome => "Google Chrome",
        }
    }
    pub fn ext_page(self) -> &'static str {
        match self {
            Self::Edge => "edge://extensions/",
            Self::Chrome => "chrome://extensions/",
        }
    }
    fn candidates(self) -> Vec<PathBuf> {
        let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
        let rel = match self {
            Self::Edge => r"Microsoft\Edge\Application\msedge.exe",
            Self::Chrome => r"Google\Chrome\Application\chrome.exe",
        };
        ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"].iter().filter_map(|k| env(k)).map(|b| b.join(rel)).collect()
    }
}

/// 检测已安装的浏览器
pub fn detect() -> Vec<(Browser, PathBuf)> {
    [Browser::Edge, Browser::Chrome]
        .into_iter()
        .filter_map(|b| b.candidates().into_iter().find(|p| p.exists()).map(|p| (b, p)))
        .collect()
}

/// 打开浏览器扩展页，并在资源管理器中选中扩展文件夹
pub fn launch(exe: &PathBuf, b: Browser, token: &str, port: u16) -> std::io::Result<PathBuf> {
    let dir = extract(token, port)?;
    std::process::Command::new(exe).arg(b.ext_page()).spawn()?;
    let mut arg = std::ffi::OsString::from("/select,");
    arg.push(&dir);
    let _ = std::process::Command::new("explorer").arg(arg).spawn();
    Ok(dir)
}
