//! 界面侧的 Windows 集成：系统通知（Toast）、任务栏进度、拖放链接。
use crate::engine::{AddReq, UiEvent, store};
use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use windows::Win32::Foundation::{HWND, POINTL};
use windows::core::{HSTRING, implement};

pub const AUMID: &str = "SwiftDL.Downloader";

// ───────────── 应用标识与图标 ─────────────

/// 最小 PNG 编码（未压缩 deflate），用于通知图标
fn png(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend(crc(&c).to_be_bytes());
    }
    let mut raw = Vec::with_capacity((w * h * 4 + h) as usize);
    for row in rgba.chunks(w as usize * 4) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    // zlib：头 + 存储块（每块最多 65535 字节）+ adler32
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i == blocks.len() - 1) as u8);
        z.extend((b.len() as u16).to_le_bytes());
        z.extend((!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// 注册应用标识（HKCU，无需管理员），未打包的桌面程序据此发送系统通知
fn register_aumid() -> Option<()> {
    use windows::Win32::System::Registry::*;
    let icon = store::data_dir().join("swiftdl.png");
    if !icon.exists() {
        let d = crate::ui::app_icon();
        let _ = std::fs::create_dir_all(store::data_dir());
        let _ = std::fs::write(&icon, png(&d.rgba, d.width, d.height));
    }
    let key = HSTRING::from(format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
    let set = |name: &str, val: &str| unsafe {
        let v: Vec<u16> = val.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2);
        RegSetKeyValueW(HKEY_CURRENT_USER, &key, &HSTRING::from(name), REG_SZ.0, Some(bytes.as_ptr() as _), bytes.len() as u32)
    };
    (set("DisplayName", "SwiftDL").is_ok() && set("IconUri", &icon.display().to_string()).is_ok()).then_some(())
}

// ───────────── 系统通知 ─────────────

pub struct Notifier {
    tx: Sender<(String, String)>,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn show_toast(title: &str, body: &str) -> windows::core::Result<()> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    )))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)
}

impl Notifier {
    pub fn new() -> Self {
        let _ = register_aumid();
        unsafe {
            let _ = windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(AUMID));
        }
        let (tx, rx) = channel::<(String, String)>();
        std::thread::spawn(move || {
            unsafe {
                let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
            }
            for (t, b) in rx {
                let _ = show_toast(&t, &b);
            }
        });
        Self { tx }
    }

    pub fn send(&self, title: &str, body: &str) {
        let _ = self.tx.send((title.to_string(), body.to_string()));
    }
}

// ───────────── 任务栏进度 ─────────────

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Progress {
    None,
    Normal(f32),
    Paused(f32),
    Error(f32),
}

pub struct Taskbar {
    list: windows::Win32::UI::Shell::ITaskbarList3,
    hwnd: HWND,
    last: Option<(u8, u32)>,
}

impl Taskbar {
    pub fn new(hwnd: HWND) -> Option<Self> {
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
        use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
        let list: ITaskbarList3 = unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok()? };
        unsafe { list.HrInit().ok()? };
        Some(Self { list, hwnd, last: None })
    }

    pub fn set(&mut self, p: Progress) {
        use windows::Win32::UI::Shell::{TBPF_ERROR, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED};
        let (state, frac) = match p {
            Progress::None => (0u8, 0.0),
            Progress::Normal(f) => (1, f),
            Progress::Paused(f) => (2, f),
            Progress::Error(f) => (3, f),
        };
        let key = (state, (frac.clamp(0.0, 1.0) * 1000.0) as u32);
        if self.last == Some(key) {
            return;
        }
        self.last = Some(key);
        let flag = [TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED, TBPF_ERROR][state as usize];
        unsafe {
            let _ = self.list.SetProgressState(self.hwnd, flag);
            if state != 0 {
                let _ = self.list.SetProgressValue(self.hwnd, key.1 as u64, 1000);
            }
        }
    }
}

// ───────────── 拖放 ─────────────

/// 从拖放的文本与文件中提取下载链接（.txt 链接列表、.url 快捷方式）
pub fn links_from_drop(text: Option<&str>, files: &[PathBuf]) -> Vec<String> {
    let schemes = ["http://", "https://", "ftp://", "ftps://", "sftp://", "magnet:?"];
    let mut out: Vec<String> = Vec::new();
    let mut torrents: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim().trim_matches(|c| c == '"' || c == '\'' || c == '<' || c == '>');
        if schemes.iter().any(|p| s.to_ascii_lowercase().starts_with(p)) && !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    };
    if let Some(t) = text {
        t.split_whitespace().for_each(&mut push);
    }
    for f in files {
        let ext = f.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let read = || std::fs::read(f).ok().filter(|b| b.len() < 4 << 20).map(|b| String::from_utf8_lossy(&b).into_owned());
        match ext.as_str() {
            "txt" | "lst" => {
                if let Some(t) = read() {
                    t.split_whitespace().for_each(&mut push);
                }
            }
            "torrent" => {
                if let Ok(u) = url::Url::from_file_path(f) {
                    torrents.push(u.to_string());
                }
            }
            "url" => {
                if let Some(t) = read() {
                    t.lines().filter_map(|l| l.trim().strip_prefix("URL=")).for_each(&mut push);
                }
            }
            _ => {}
        }
    }
    out.extend(torrents);
    out
}

#[implement(windows::Win32::System::Ole::IDropTarget)]
struct DropTarget {
    tx: Sender<UiEvent>,
    ctx: egui::Context,
    over: Arc<AtomicBool>,
}

fn get_text(data: &windows::Win32::System::Com::IDataObject, fmt: u16) -> Option<String> {
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::ReleaseStgMedium;
    let fe = FORMATETC { cfFormat: fmt, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
    unsafe {
        let mut med = data.GetData(&fe).ok()?;
        let h = med.u.hGlobal;
        let p = GlobalLock(h) as *const u16;
        let r = (!p.is_null()).then(|| {
            let n = GlobalSize(h) / 2;
            let s = std::slice::from_raw_parts(p, n);
            let end = s.iter().position(|&c| c == 0).unwrap_or(n);
            String::from_utf16_lossy(&s[..end])
        });
        let _ = GlobalUnlock(h);
        ReleaseStgMedium(&mut med);
        r
    }
}

fn get_files(data: &windows::Win32::System::Com::IDataObject) -> Vec<PathBuf> {
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Ole::{CF_HDROP, ReleaseStgMedium};
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
    let fe = FORMATETC { cfFormat: CF_HDROP.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
    let mut out = Vec::new();
    unsafe {
        let Ok(mut med) = data.GetData(&fe) else { return out };
        let hdrop = HDROP(med.u.hGlobal.0);
        let n = DragQueryFileW(hdrop, u32::MAX, None);
        for i in 0..n.min(200) {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            out.push(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
        }
        ReleaseStgMedium(&mut med);
    }
    out
}

fn extract(data: &windows::Win32::System::Com::IDataObject) -> Vec<String> {
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    let url_fmt = unsafe { RegisterClipboardFormatW(&HSTRING::from("UniformResourceLocatorW")) } as u16;
    let text = get_text(data, url_fmt).or_else(|| get_text(data, CF_UNICODETEXT.0));
    links_from_drop(text.as_deref(), &get_files(data))
}

impl windows::Win32::System::Ole::IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        _data: windows::core::Ref<windows::Win32::System::Com::IDataObject>,
        _keys: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut windows::Win32::System::Ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.over.store(true, Ordering::Relaxed);
        self.ctx.request_repaint();
        unsafe { *effect = windows::Win32::System::Ole::DROPEFFECT_COPY };
        Ok(())
    }
    fn DragOver(
        &self,
        _keys: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut windows::Win32::System::Ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe { *effect = windows::Win32::System::Ole::DROPEFFECT_COPY };
        Ok(())
    }
    fn DragLeave(&self) -> windows::core::Result<()> {
        self.over.store(false, Ordering::Relaxed);
        self.ctx.request_repaint();
        Ok(())
    }
    fn Drop(
        &self,
        data: windows::core::Ref<windows::Win32::System::Com::IDataObject>,
        _keys: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut windows::Win32::System::Ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.over.store(false, Ordering::Relaxed);
        let links = data.as_ref().map(extract).unwrap_or_default();
        unsafe { *effect = windows::Win32::System::Ole::DROPEFFECT_COPY };
        if !links.is_empty() {
            let reqs = links.into_iter().map(|url| AddReq { url, ..Default::default() }).collect();
            let _ = self.tx.send(UiEvent::Add(reqs));
        }
        self.ctx.request_repaint();
        Ok(())
    }
}

/// 在主窗口注册拖放目标（需在 UI 线程、窗口创建后调用）；返回“拖放悬停中”标志
pub fn register_drop(hwnd: HWND, tx: Sender<UiEvent>, ctx: egui::Context) -> Option<Arc<AtomicBool>> {
    use windows::Win32::System::Ole::{IDropTarget, OleInitialize, RegisterDragDrop};
    let over = Arc::new(AtomicBool::new(false));
    let target: IDropTarget = DropTarget { tx, ctx, over: over.clone() }.into();
    unsafe {
        let _ = OleInitialize(None);
        RegisterDragDrop(hwnd, &target).ok()?;
    }
    // 目标对象由系统持有引用，窗口销毁时随之释放
    std::mem::forget(target);
    Some(over)
}

/// 从 eframe 的窗口句柄得到 HWND
pub fn hwnd_of(h: &impl raw_window_handle::HasWindowHandle) -> Option<HWND> {
    match h.window_handle().ok()?.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(w) => Some(HWND(w.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// 主窗口句柄（作为“打开方式”等系统对话框的父窗口）
static MAIN_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

pub fn set_main_hwnd(h: HWND) {
    MAIN_HWND.store(h.0 as isize, Ordering::Relaxed);
}

/// 打开已下载的文件：`choose` 为 false 时用系统默认程序打开，没有关联程序则弹出“打开方式”让用户选择；
/// `choose` 为 true 时直接让用户选择。在后台线程执行，系统对话框不会卡住界面。
pub fn open_file(path: &std::path::Path, choose: bool) {
    let path = path.to_path_buf();
    let owner = MAIN_HWND.load(Ordering::Relaxed);
    std::thread::spawn(move || {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize};
        use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let owner = (owner != 0).then_some(HWND(owner as *mut _));
        // ShellExecuteEx 与“打开方式”对话框都要求 STA 线程
        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok();
        let r = if !path.exists() {
            Err(trf!("文件不存在：{}", path.display()))
        } else if choose {
            open_with_dialog(&path, owner)
        } else {
            match shell_open(&path, owner) {
                Err(OpenErr::NoAssoc) => open_with_dialog(&path, owner),
                Err(OpenErr::Other(e)) => Err(e),
                Ok(()) => Ok(()),
            }
        };
        if let Err(e) = r {
            unsafe { MessageBoxW(owner, &HSTRING::from(e), &HSTRING::from(tr!("无法打开文件")), MB_OK | MB_ICONERROR) };
        }
        if com {
            unsafe { CoUninitialize() };
        }
    });
}

enum OpenErr {
    /// 该类型文件没有关联的程序
    NoAssoc,
    Other(String),
}

/// 用文件类型的默认操作打开（等同于在资源管理器中双击）
fn shell_open(path: &std::path::Path, owner: Option<HWND>) -> Result<(), OpenErr> {
    use windows::Win32::Foundation::ERROR_NO_ASSOCIATION;
    use windows::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let file = HSTRING::from(path.as_os_str());
    let dir = path.parent().map(|d| HSTRING::from(d.as_os_str())).unwrap_or_default();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        // 不让系统自己弹错误框：没有关联程序时由我们打开“打开方式”
        fMask: SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
        hwnd: owner.unwrap_or_default(),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        lpDirectory: windows::core::PCWSTR(dir.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => Ok(()),
        // SE_ERR_NOASSOC = 31
        Err(e) if e.code() == ERROR_NO_ASSOCIATION.to_hresult() || info.hInstApp.0 as usize == 31 => Err(OpenErr::NoAssoc),
        Err(e) => Err(OpenErr::Other(e.message())),
    }
}

/// 系统“打开方式”对话框：用户选择程序后直接打开，可勾选“始终使用此应用”
fn open_with_dialog(path: &std::path::Path, owner: Option<HWND>) -> Result<(), String> {
    use windows::Win32::Foundation::ERROR_CANCELLED;
    use windows::Win32::UI::Shell::{OAIF_ALLOW_REGISTRATION, OAIF_EXEC, OAIF_REGISTER_EXT, OPENASINFO, SHOpenWithDialog};
    let file = HSTRING::from(path.as_os_str());
    let info = OPENASINFO {
        pcszFile: windows::core::PCWSTR(file.as_ptr()),
        pcszClass: windows::core::PCWSTR::null(),
        oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_REGISTER_EXT | OAIF_EXEC,
    };
    match unsafe { SHOpenWithDialog(owner, &info) } {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => Ok(()), // 用户取消
        Err(e) => Err(e.message()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_parsing() {
        let t = "看这个 https://a.com/x.zip 和 <https://b.com/y.mp4>\n\"ftp://h/f.iso\" magnet:?xt=urn:btih:abc 不是链接 https://a.com/x.zip";
        assert_eq!(
            links_from_drop(Some(t), &[]),
            ["https://a.com/x.zip", "https://b.com/y.mp4", "ftp://h/f.iso", "magnet:?xt=urn:btih:abc"]
        );
        let dir = std::env::temp_dir().join(format!("swiftdl-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let txt = dir.join("list.txt");
        std::fs::write(&txt, "https://c.com/1.bin\r\nhttps://c.com/2.bin\r\n").unwrap();
        let url = dir.join("site.url");
        std::fs::write(&url, "[InternetShortcut]\r\nURL=https://d.com/file.7z\r\n").unwrap();
        let other = dir.join("photo.jpg");
        std::fs::write(&other, "x").unwrap();
        assert_eq!(links_from_drop(None, &[txt, url, other]), ["https://c.com/1.bin", "https://c.com/2.bin", "https://d.com/file.7z"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn png_is_valid() {
        let rgba = vec![200u8; 64 * 64 * 4];
        let p = png(&rgba, 64, 64);
        assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
        assert!(p.windows(4).any(|w| w == b"IEND"));
        assert!(p.len() > 64 * 64 * 4);
    }
}

/// 手动运行：cargo test toast_smoke -- --ignored（会在桌面弹出一条测试通知）
#[cfg(test)]
#[test]
#[ignore]
fn toast_smoke() {
    register_aumid().expect("注册应用标识");
    unsafe {
        let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
    }
    show_toast("SwiftDL 测试通知", "系统通知工作正常").expect("发送通知");
}
