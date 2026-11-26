//! 清除软件数据（便于用户卸载）：注册表项、未完成任务的临时文件，以及 `%APPDATA%\swiftdl` 整个目录。
//!
//! 运行中的程序占用着任务库、日志与 BT 会话文件，而且退出时还会保存设置，因此数据目录由一个辅助进程
//! （`swiftdl.exe --purge-data <pid>`）在主程序退出后删除。已下载完成的文件不受影响。
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const ARG: &str = "--purge-data";
const NO_WINDOW: u32 = 0x0800_0000;

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// 用户确认清除后调用，随后退出程序
pub fn request() {
    REQUESTED.store(true, Ordering::Relaxed);
}

pub fn requested() -> bool {
    REQUESTED.load(Ordering::Relaxed)
}

fn reg(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("reg").args(args).creation_flags(NO_WINDOW).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 删除本程序写入的注册表项（均在 HKCU）；只删除确实属于 SwiftDL 的关联
pub fn clear_registry() {
    crate::tray::set_autostart(false);
    let k = r"HKCU\Software\Classes";
    let ours = |key: &str| reg(&["query", key, "/ve"]).is_some_and(|o| o.to_ascii_lowercase().contains("swiftdl"));
    if ours(&format!(r"{k}\magnet\shell\open\command")) {
        reg(&["delete", &format!(r"{k}\magnet"), "/f"]);
    }
    if ours(&format!(r"{k}\.torrent")) {
        reg(&["delete", &format!(r"{k}\.torrent"), "/ve", "/f"]); // 只删默认值，保留其他程序的 OpenWithProgids
    }
    reg(&["delete", &format!(r"{k}\SwiftDL.Torrent"), "/f"]);
    reg(&["delete", &format!(r"{k}\AppUserModelId\{}", crate::winsys::AUMID), "/f"]);
    log_info!("purge: registry entries removed");
}

/// 删除未完成任务的临时文件（下载目录中的 `.swdl`）；需在任务全部停止后调用
pub fn remove_part_files(parts: &[PathBuf]) {
    for p in parts {
        let _ = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
    }
}

/// 启动辅助进程：等待本进程退出后删除数据目录
pub fn spawn_helper() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    std::process::Command::new(exe).arg(ARG).arg(std::process::id().to_string()).spawn().is_ok()
}

/// 等待指定进程退出（最多 `timeout`）
fn wait_pid(pid: u32, timeout: Duration) {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return; // 已退出
        }
        let _ = WaitForSingleObject(h, timeout.as_millis() as u32) == WAIT_OBJECT_0;
        CloseHandle(h);
    }
}

/// 反复尝试删除目录：杀毒软件或索引服务可能短暂占用其中的文件
fn remove_dir_retry(dir: &Path, timeout: Duration) -> std::io::Result<()> {
    let start = Instant::now();
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if start.elapsed() >= timeout => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(300)),
        }
    }
}

/// 辅助进程入口：不初始化日志，也不读写任何设置，删除完成后提示用户
pub fn run_helper(pid: Option<u32>) {
    if let Some(pid) = pid {
        wait_pid(pid, Duration::from_secs(60));
    }
    let dir = crate::engine::store::data_dir();
    let (level, text) = match remove_dir_retry(&dir, Duration::from_secs(20)) {
        Ok(()) => {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::fs::remove_file(exe.with_extension("exe.old"));
            }
            (rfd::MessageLevel::Info, tr!("SwiftDL 的所有数据已清除。现在可以直接删除程序文件；浏览器中的 SwiftDL 扩展需在扩展管理页中手动移除。").to_string())
        }
        Err(e) => (rfd::MessageLevel::Error, trf!("部分数据未能删除（{}），请手动删除文件夹：\n{}", e, dir.display())),
    };
    rfd::MessageDialog::new().set_level(level).set_title("SwiftDL").set_description(text).show();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_dir_and_parts() {
        let root = std::env::temp_dir().join(format!("swiftdl-purge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("swiftdl");
        std::fs::create_dir_all(data.join("logs")).unwrap();
        std::fs::write(data.join("settings.json"), b"{}").unwrap();
        std::fs::write(data.join("logs").join("a.log"), b"x").unwrap();
        remove_dir_retry(&data, Duration::from_secs(1)).unwrap();
        assert!(!data.exists());
        // 目录不存在也算成功
        remove_dir_retry(&data, Duration::from_secs(1)).unwrap();

        let dl = root.join("downloads");
        std::fs::create_dir_all(&dl).unwrap();
        let (part, done) = (dl.join("a.iso.swdl"), dl.join("b.iso"));
        std::fs::write(&part, b"partial").unwrap();
        std::fs::write(&done, b"finished").unwrap();
        remove_part_files(&[part.clone(), dl.join("missing.swdl")]);
        assert!(!part.exists());
        assert!(done.exists(), "已下载完成的文件不应被删除");
        let _ = std::fs::remove_dir_all(&root);
    }
}
