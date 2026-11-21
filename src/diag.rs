//! 诊断：按天滚动的运行日志与崩溃报告，位于 `%APPDATA%\swiftdl\logs`。
//! 只记录脱敏后的信息：链接去掉账号密码、查询参数与片段（签名链接的令牌常在查询参数里），不记录 Cookie 与请求头。
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// 日志保留天数
const KEEP_DAYS: usize = 7;
/// 崩溃报告保留份数
const KEEP_CRASHES: usize = 10;

/// 只有正式运行（`init`）才写文件；单元测试不产生日志文件
static ENABLED: AtomicBool = AtomicBool::new(false);
static SINK: Mutex<Option<(String, std::fs::File)>> = Mutex::new(None);

pub fn dir() -> PathBuf {
    crate::engine::store::data_dir().join("logs")
}

/// 本地时间：("2026-11-21", "13:05:09.123")
fn stamp() -> (String, String) {
    let mut t = unsafe { std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    (
        format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay),
        format!("{:02}:{:02}:{:02}.{:03}", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds),
    )
}

/// 开启日志并安装崩溃处理；启动时调用一次
pub fn init() {
    ENABLED.store(true, Ordering::Relaxed);
    let _ = std::fs::create_dir_all(dir());
    install_panic_hook();
}

pub fn write(level: &str, msg: &str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let (day, time) = stamp();
    let mut g = SINK.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_none_or(|(d, _)| *d != day) {
        let d = dir();
        let _ = std::fs::create_dir_all(&d);
        *g = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(d.join(format!("swiftdl-{day}.log")))
            .ok()
            .map(|f| (day.clone(), f));
        prune(&d, "swiftdl-", ".log", KEEP_DAYS);
    }
    if let Some((_, f)) = g.as_mut() {
        let _ = writeln!(f, "{time} {level:5} {}", scrub(msg));
    }
}

macro_rules! log_info {
    ($($a:tt)*) => { $crate::diag::write("INFO", &format!($($a)*)) };
}
macro_rules! log_warn {
    ($($a:tt)*) => { $crate::diag::write("WARN", &format!($($a)*)) };
}
macro_rules! log_error {
    ($($a:tt)*) => { $crate::diag::write("ERROR", &format!($($a)*)) };
}

/// 删除多余的旧文件（文件名含日期/时间，按名称排序即按时间排序）
fn prune(d: &Path, prefix: &str, suffix: &str, keep: usize) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(prefix) && n.ends_with(suffix)))
        .collect();
    v.sort();
    let n = v.len().saturating_sub(keep);
    for p in &v[..n] {
        let _ = std::fs::remove_file(p);
    }
}

/// 链接脱敏：只保留协议、主机、端口和路径；磁力链接只保留 info hash
pub fn url(u: &str) -> String {
    if let Some(rest) = u.strip_prefix("magnet:?") {
        let hash = rest.split('&').find_map(|kv| kv.strip_prefix("xt=")).unwrap_or("");
        return format!("magnet:?xt={hash}");
    }
    match url::Url::parse(u) {
        Ok(mut p) => {
            let had_query = p.query().is_some();
            let _ = p.set_username("");
            let _ = p.set_password(None);
            p.set_query(None);
            p.set_fragment(None);
            let mut s = p.to_string();
            if had_query {
                s.push_str("?...");
            }
            s
        }
        Err(_) => "<url>".into(),
    }
}

/// 把消息中出现的链接全部脱敏（错误信息里常带完整链接）
pub fn scrub(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    loop {
        let next = ["https://", "http://", "ftps://", "ftp://", "sftp://", "magnet:?"].iter().filter_map(|k| rest.find(k)).min();
        let Some(i) = next else { break };
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '<' | '>')).unwrap_or(tail.len());
        out.push_str(&url(&tail[..end]));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let (day, time) = stamp();
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let bt = std::backtrace::Backtrace::force_capture();
        let report = format!(
            "SwiftDL v{} crash report\n{day} {time}\nWindows build: {}\nthread: {thread}\n\n{}\n\nbacktrace:\n{bt}\n",
            env!("CARGO_PKG_VERSION"),
            os_build(),
            scrub(&info.to_string()),
        );
        let d = dir();
        let _ = std::fs::create_dir_all(&d);
        let name = format!("crash-{day}-{}.txt", time.replace([':', '.'], ""));
        let _ = std::fs::write(d.join(name), report);
        prune(&d, "crash-", ".txt", KEEP_CRASHES);
        write("ERROR", &format!("panic in thread {thread}: {info}"));
        default(info);
    }));
}

/// Windows 版本号（如 10.0.20348），写入崩溃报告
fn os_build() -> String {
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32;
    }
    let mut v = unsafe { std::mem::zeroed::<OSVERSIONINFOW>() };
    v.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    if unsafe { RtlGetVersion(&mut v) } == 0 {
        format!("{}.{}.{}", v.dwMajorVersion, v.dwMinorVersion, v.dwBuildNumber)
    } else {
        "unknown".into()
    }
}

/// 上次运行留下、尚未提示过的崩溃报告（最新一份）；调用后全部标记为已提示
pub fn take_crash_report() -> Option<PathBuf> {
    let d = dir();
    let mut new: Vec<PathBuf> = std::fs::read_dir(&d)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("crash-") && n.ends_with(".txt") && !n.ends_with(".seen.txt"))
        })
        .collect();
    new.sort();
    let mut latest = None;
    for p in new {
        let seen = p.with_extension("seen.txt");
        if std::fs::rename(&p, &seen).is_ok() {
            latest = Some(seen);
        }
    }
    latest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact() {
        assert_eq!(url("https://user:pw@cdn.com:8443/a/b.zip?token=SECRET#x"), "https://cdn.com:8443/a/b.zip?...");
        assert_eq!(url("ftp://bob:pw@h.com/f.iso"), "ftp://h.com/f.iso");
        assert_eq!(url("magnet:?xt=urn:btih:abc&dn=name&tr=udp://t"), "magnet:?xt=urn:btih:abc");
        let m = scrub("error sending request for url (https://a.com/x?sig=SECRET): timed out; retry ftp://u:p@b.com/y");
        assert_eq!(m, "error sending request for url (https://a.com/x?...): timed out; retry ftp://b.com/y");
        assert!(!m.contains("SECRET"));
        assert_eq!(scrub("纯文本"), "纯文本");
    }

    #[test]
    fn files_and_crash_reports() {
        let root = std::env::temp_dir().join(format!("swiftdl-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        unsafe { std::env::set_var("APPDATA", &root) };
        let d = dir();
        std::fs::create_dir_all(&d).unwrap();

        // 旧日志只保留最近几天
        for i in 0..10 {
            std::fs::write(d.join(format!("swiftdl-2020-01-{:02}.log", i + 1)), "").unwrap();
        }
        prune(&d, "swiftdl-", ".log", KEEP_DAYS);
        let logs: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
        assert_eq!(logs.len(), KEEP_DAYS);
        assert!(!d.join("swiftdl-2020-01-01.log").exists() && d.join("swiftdl-2020-01-10.log").exists());

        // 崩溃报告：只提示一次，返回最新一份
        assert!(take_crash_report().is_none());
        std::fs::write(d.join("crash-2026-01-01-100000000.txt"), "a").unwrap();
        std::fs::write(d.join("crash-2026-01-02-100000000.txt"), "b").unwrap();
        let p = take_crash_report().unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "b");
        assert!(take_crash_report().is_none(), "提示过的报告不再提示");

        assert!(os_build().starts_with("10."));

        // 实际写入：日志行带时间与级别，链接已脱敏
        ENABLED.store(true, Ordering::Relaxed);
        write("INFO", "task 1 start: https://u:pw@a.com/x.zip?token=SECRET");
        ENABLED.store(false, Ordering::Relaxed);
        *SINK.lock().unwrap() = None; // 关闭文件句柄，便于删除临时目录
        let (day, _) = stamp();
        let log = std::fs::read_to_string(d.join(format!("swiftdl-{day}.log"))).unwrap();
        assert!(log.trim_end().ends_with("INFO  task 1 start: https://a.com/x.zip?..."), "{log}");
        assert!(!log.contains("SECRET") && !log.contains("pw"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
