//! 系统托盘：关闭窗口后台继续下载；左键单击恢复窗口，右键菜单。
use eframe::egui;
use std::sync::mpsc::{Receiver, channel};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayCmd {
    Show,
    StartAll,
    PauseAll,
    Quit,
}

pub struct Tray {
    pub icon: TrayIcon,
    pub rx: Receiver<TrayCmd>,
}

/// 必须在主线程（窗口事件循环所在线程）创建
pub fn create(ctx: &egui::Context) -> Option<Tray> {
    let show = MenuItem::new(tr!("显示窗口"), true, None);
    let start = MenuItem::new(tr!("全部开始"), true, None);
    let pause = MenuItem::new(tr!("全部暂停"), true, None);
    let quit = MenuItem::new(tr!("退出 SwiftDL"), true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &show,
        &PredefinedMenuItem::separator(),
        &start,
        &pause,
        &PredefinedMenuItem::separator(),
        &quit,
    ])
    .ok()?;

    let data = crate::ui::app_icon();
    let icon = tray_icon::Icon::from_rgba(data.rgba, data.width, data.height).ok()?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tr!("SwiftDL 下载器"))
        .with_icon(icon)
        .with_menu_on_left_click(false)
        .build()
        .ok()?;

    let (tx, rx) = channel();
    let ids = [
        (show.id().clone(), TrayCmd::Show),
        (start.id().clone(), TrayCmd::StartAll),
        (pause.id().clone(), TrayCmd::PauseAll),
        (quit.id().clone(), TrayCmd::Quit),
    ];
    let (tx2, c1, c2) = (tx.clone(), ctx.clone(), ctx.clone());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Some((_, cmd)) = ids.iter().find(|(id, _)| *id == e.id) {
            let _ = tx.send(*cmd);
            c1.request_repaint();
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            let _ = tx2.send(TrayCmd::Show);
            c2.request_repaint();
        }
    }));
    Some(Tray { icon: tray, rx })
}

/// 开机自启动（当前用户）：写入/删除 HKCU Run 键
pub fn set_autostart(on: bool) -> bool {
    use std::os::windows::process::CommandExt;
    const NO_WINDOW: u32 = 0x0800_0000;
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let mut cmd = std::process::Command::new("reg");
    if on {
        let Ok(exe) = std::env::current_exe() else { return false };
        cmd.args(["add", key, "/v", "SwiftDL", "/t", "REG_SZ", "/f", "/d"]).arg(format!("\"{}\" --minimized", exe.display()));
    } else {
        cmd.args(["delete", key, "/v", "SwiftDL", "/f"]);
    }
    cmd.creation_flags(NO_WINDOW).status().is_ok_and(|s| s.success())
}
