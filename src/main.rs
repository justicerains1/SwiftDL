#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod i18n;
mod i18n_en;
#[macro_use]
mod diag;

mod browser;
mod clipboard;
mod engine;
mod purge;
mod server;
mod tray;
mod update;
mod ui;
mod winsys;

use engine::{AddReq, Manager, UiEvent};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    // 清除数据的辅助进程：等主程序退出后删除数据目录；不能初始化日志，否则会重新创建数据目录
    let mut raw = std::env::args().skip(1);
    if raw.next().as_deref() == Some(purge::ARG) {
        i18n::apply(engine::store::load_settings().lang);
        purge::run_helper(raw.next().and_then(|p| p.parse().ok()));
        return;
    }
    diag::init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let minimized = args.iter().any(|a| a == "--minimized");
    let updated = args.iter().any(|a| a == "--updated");
    update::cleanup_old();
    // 命令行：http(s)/ftp/sftp 链接、磁力链接、.torrent 文件（文件关联打开时）
    let urls: Vec<String> = args
        .into_iter()
        .filter_map(|a| {
            let l = a.to_ascii_lowercase();
            if ["http://", "https://", "ftp://", "ftps://", "sftp://", "magnet:?"].iter().any(|p| l.starts_with(p)) {
                Some(a)
            } else if l.ends_with(".torrent") && std::path::Path::new(&a).is_file() {
                url::Url::from_file_path(std::fs::canonicalize(&a).unwrap_or(a.into())).ok().map(|u| u.to_string())
            } else {
                None
            }
        })
        .collect();
    let settings = engine::store::load_settings();
    i18n::apply(settings.lang);
    let port = settings.port;

    // 单实例：端口被占用且对方是 swiftdl → 转交参数后退出
    // 刚完成自动更新：旧进程可能还没退出，稍等它释放端口
    let bind = || rt.block_on(tokio::net::TcpListener::bind(("127.0.0.1", port)));
    let mut first = bind();
    if updated {
        for _ in 0..40 {
            if first.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
            first = bind();
        }
    }
    let listener = match first {
        Ok(l) => Some(l),
        Err(_) => {
            if rt.block_on(server::forward(port, &settings.token, &urls)) {
                return;
            }
            None
        }
    };

    log_info!(
        "SwiftDL v{} started (minimized={minimized}, updated={updated}, server={}, urls={})",
        env!("CARGO_PKG_VERSION"),
        listener.is_some(),
        urls.len()
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let m = Manager::new(rt.handle().clone(), settings);
    {
        // 已释放过的扩展文件夹：同步最新的配对令牌，用户无需重新配对
        let s = m.settings.read().unwrap();
        browser::refresh_config(&s.token, s.port);
    }
    if let Some(l) = listener {
        rt.spawn(server::serve(l, tx.clone(), m.clone()));
    }
    clipboard::spawn(tx.clone(), m.clone());
    if !urls.is_empty() {
        let _ = tx.send(UiEvent::Add(urls.into_iter().map(|url| AddReq { url, ..Default::default() }).collect()));
    }

    // 后台节拍：测速 / 保存 / 调度 / 定期清缓存
    let bg = m.clone();
    rt.spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(500));
        let mut n: u64 = 0;
        let mut last_wall = std::time::SystemTime::now();
        loop {
            iv.tick().await;
            n += 1;
            // 睡眠唤醒（墙钟跳变）与网络恢复：立即重建连接 / 重试网络错误
            let wall = std::time::SystemTime::now();
            let gap = wall.duration_since(last_wall).map(|d| d.as_secs()).unwrap_or(0);
            last_wall = wall;
            if engine::sys::woke_from_sleep(gap) {
                log_info!("woke from sleep after {gap}s, reconnecting");
                bg.on_wake();
            } else if engine::sys::NET_RESTORED.swap(false, std::sync::atomic::Ordering::Relaxed) {
                log_info!("network restored, retrying network errors");
                bg.on_network_restored();
            }
            let running = bg.tick();
            // 下载中每 3 秒落盘一次（内容未变则跳过），空闲时仅在有改动时保存
            if n.is_multiple_of(6) || (!running && n.is_multiple_of(2)) {
                let c = bg.clone();
                tokio::task::spawn_blocking(move || c.save_if_needed(running));
            }
            if n == 10 || n.is_multiple_of(3600) {
                let c = bg.clone();
                tokio::task::spawn_blocking(move || c.cleanup());
            }
        }
    });
    m.schedule();
    engine::sys::watch_network();

    let app_m = m.clone();
    let ui_tx = tx.clone();
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("SwiftDL")
            .with_inner_size([1100.0, 680.0])
            .with_min_inner_size([760.0, 460.0])
            .with_icon(Arc::new(ui::app_icon()))
            // 使用自定义拖放目标（支持链接与文本），关闭 winit 自带的仅文件拖放
            .with_drag_and_drop(false),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("SwiftDL", opts, Box::new(move |cc| Ok(Box::new(ui::App::new(cc, app_m, rx, ui_tx, minimized))))) {
        eprintln!("界面启动失败: {e}");
        log_error!("UI failed to start: {e}");
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("SwiftDL")
            .set_description(trf!("界面启动失败：{}", e))
            .show();
    }

    // 退出：停止下载、刷盘，下次启动自动续传原先在下载的任务
    let running: Vec<_> = m.tasks.read().unwrap().iter().filter(|t| t.status() == engine::Status::Running).cloned().collect();
    m.settings.write().unwrap().max_active = 0; // 阻止调度器拉起新任务
    for t in &running {
        t.cancel.lock().unwrap().cancel();
    }
    rt.block_on(async {
        for _ in 0..60 {
            if running.iter().all(|t| t.status() != engine::Status::Running) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    if purge::requested() {
        // 清除数据：删除未完成任务的临时文件，不再保存任何状态，由辅助进程在退出后删除数据目录
        let parts: Vec<_> = m.tasks.read().unwrap().iter().filter(|t| t.status() != engine::Status::Done).map(|t| t.part_path()).collect();
        purge::remove_part_files(&parts);
        purge::clear_registry();
        log_info!("purge: exiting, helper started={}", purge::spawn_helper());
        return;
    }
    for t in &running {
        t.set_status(engine::Status::Queued);
    }
    m.save();
    log_info!("exit ({} running task(s) will resume next start)", running.len());
}
