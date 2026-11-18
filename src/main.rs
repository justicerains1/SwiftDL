#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clipboard;
mod engine;
mod server;
mod ui;

use engine::{AddReq, Manager, UiEvent};
use std::time::Duration;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let urls: Vec<String> = std::env::args().skip(1).filter(|a| a.starts_with("http")).collect();
    let settings = engine::store::load_settings();
    let port = settings.port;

    // 单实例：端口被占用且对方是 swiftdl → 转交参数后退出
    let listener = match rt.block_on(tokio::net::TcpListener::bind(("127.0.0.1", port))) {
        Ok(l) => Some(l),
        Err(_) => {
            if rt.block_on(server::forward(port, &urls)) {
                return;
            }
            None
        }
    };

    let (tx, rx) = std::sync::mpsc::channel();
    let m = Manager::new(rt.handle().clone(), settings);
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
        loop {
            iv.tick().await;
            n += 1;
            let running = bg.tick();
            if n.is_multiple_of(2) {
                bg.save_if_needed(running);
            }
            if n == 10 || n.is_multiple_of(3600) {
                let c = bg.clone();
                tokio::task::spawn_blocking(move || c.cleanup());
            }
        }
    });
    m.schedule();

    let app_m = m.clone();
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("SwiftDL")
            .with_inner_size([960.0, 560.0])
            .with_min_inner_size([640.0, 360.0]),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("SwiftDL", opts, Box::new(move |cc| Ok(Box::new(ui::App::new(cc, app_m, rx))))) {
        eprintln!("界面启动失败: {e}");
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("SwiftDL")
            .set_description(format!("界面启动失败：{e}"))
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
    for t in &running {
        t.set_status(engine::Status::Queued);
    }
    m.save();
}
