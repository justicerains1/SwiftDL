//! 剪贴板监听：复制下载链接后自动弹出添加窗口。
use crate::engine::{AddReq, Manager, UiEvent};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

pub fn spawn(tx: Sender<UiEvent>, m: Arc<Manager>) {
    std::thread::spawn(move || {
        let mut cb: Option<arboard::Clipboard> = None;
        let mut last: Option<String> = None;
        let mut seq = 0u32;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let (on, exts) = {
                let s = m.settings.read().unwrap();
                (s.clipboard, s.exts.clone())
            };
            if !on {
                last = None;
                continue;
            }
            // 序列号未变化说明剪贴板没动过：不打开剪贴板，避免与其他程序争锁
            let cur = unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() };
            if cur == seq && last.is_some() {
                continue;
            }
            seq = cur;
            if cb.is_none() {
                cb = arboard::Clipboard::new().ok();
            }
            let Some(c) = cb.as_mut() else { continue };
            let Ok(text) = c.get_text() else { continue };
            if last.as_deref() == Some(text.as_str()) {
                continue;
            }
            // 首次读取只记录，不弹窗
            let first = last.is_none();
            last = Some(text.clone());
            if first || *m.clip_ignore.lock().unwrap() == text {
                continue;
            }
            if is_download_url(&text, &exts) {
                let _ = tx.send(UiEvent::Add(vec![AddReq { url: text.trim().to_string(), ..Default::default() }]));
                m.repaint();
            }
        }
    });
}

pub fn is_download_url(text: &str, exts: &str) -> bool {
    let t = text.trim();
    if t.contains(char::is_whitespace) || !(t.starts_with("http://") || t.starts_with("https://")) {
        return false;
    }
    let Ok(u) = url::Url::parse(t) else { return false };
    let Some(ext) = u.path().rsplit('/').next().and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()) else {
        return false;
    };
    exts.split(|c: char| c == ',' || c.is_whitespace())
        .map(|e| e.trim().trim_start_matches('.'))
        .any(|e| !e.is_empty() && e.eq_ignore_ascii_case(&ext))
}
