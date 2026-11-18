//! 通用控件：格式化、按钮、文件类型徽标、进度条、分段图、速度折线、Logo。
use super::icons::{self, Icon};
use super::theme::{self, Pal};
use crate::engine::{Status, Task};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui,
    Vec2, pos2, vec2,
};
use std::collections::VecDeque;
use std::sync::atomic::Ordering::*;

pub fn fmt_size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

pub fn fmt_speed(b: u64) -> String {
    format!("{}/s", fmt_size(b))
}

pub fn fmt_eta(secs: u64) -> String {
    match secs {
        0 => String::new(),
        s if s < 60 => trf!("{} 秒", s),
        s if s < 3600 => trf!("{} 分 {} 秒", s / 60, s % 60),
        s if s < 86400 * 3 => trf!("{} 小时 {} 分", s / 3600, s % 3600 / 60),
        _ => tr!("超过 3 天").into(),
    }
}

pub fn fmt_ago(ts: u64) -> String {
    if ts == 0 {
        return "-".into();
    }
    let d = crate::engine::now().saturating_sub(ts);
    match d {
        d if d < 60 => tr!("刚刚").into(),
        d if d < 3600 => trf!("{} 分钟前", d / 60),
        d if d < 86400 => trf!("{} 小时前", d / 3600),
        d => trf!("{} 天前", d / 86400),
    }
}

pub fn status_color(p: &Pal, s: Status) -> Color32 {
    match s {
        Status::Running => p.accent,
        Status::Done => p.success,
        Status::Error => p.danger,
        Status::Queued => p.warning,
        Status::Paused => p.weak,
    }
}

pub fn status_icon(s: Status) -> Icon {
    match s {
        Status::Running => Icon::Activity,
        Status::Done => Icon::Check,
        Status::Error => Icon::Alert,
        Status::Queued => Icon::Clock,
        Status::Paused => Icon::PauseCircle,
    }
}

/// 方形图标按钮：悬停底色、禁用半透明、带提示
pub fn icon_button(ui: &mut Ui, icon: Icon, tip: &str, enabled: bool) -> Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        if enabled && resp.hovered() {
            ui.painter().rect_filled(rect, 8, p.hover);
        }
        let c = if enabled { p.text } else { p.weak.gamma_multiply(0.45) };
        icons::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(16.0, 16.0)), icon, c);
    }
    let resp = resp.on_hover_text(tip);
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Btn {
    Primary,
    Secondary,
    Danger,
}

/// 带可选图标的按钮
pub fn button(ui: &mut Ui, kind: Btn, icon: Option<Icon>, text: &str) -> Response {
    let p = theme::pal(ui);
    let font = FontId::proportional(14.0);
    let (fill, fg, stroke) = match kind {
        Btn::Primary => (p.accent, p.on_accent, Stroke::NONE),
        Btn::Danger => (p.danger, Color32::WHITE, Stroke::NONE),
        Btn::Secondary => (p.panel, p.text, Stroke::new(1.0, p.stroke)),
    };
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, fg);
    let icon_w = if icon.is_some() { 16.0 + 6.0 } else { 0.0 };
    let size = vec2(galley.size().x + icon_w + 28.0, 34.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let mut f = fill;
        if resp.hovered() {
            f = if kind == Btn::Secondary { p.hover } else { f.gamma_multiply(0.88) };
        }
        if resp.is_pointer_button_down_on() {
            f = f.gamma_multiply(0.92);
        }
        let painter = ui.painter();
        painter.rect(rect, 8, f, stroke, StrokeKind::Inside);
        let mut x = rect.min.x + 14.0;
        if let Some(i) = icon {
            icons::paint(painter, Rect::from_min_size(pos2(x, rect.center().y - 8.0), vec2(16.0, 16.0)), i, fg);
            x += icon_w;
        }
        painter.galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 菜单项：图标 + 文字
pub fn menu_item(ui: &mut Ui, icon: Icon, text: &str, danger: bool) -> Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(180.0, 30.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6, p.hover);
    }
    let c = if danger { p.danger } else { p.text };
    icons::paint(ui.painter(), Rect::from_min_size(pos2(rect.min.x + 8.0, rect.center().y - 8.0), vec2(16.0, 16.0)), icon, c);
    ui.painter().text(pos2(rect.min.x + 34.0, rect.center().y), Align2::LEFT_CENTER, text, FontId::proportional(13.5), c);
    resp
}

/// 根据文件类型返回徽标（缩写, 颜色），分类与“按类型保存”共用 engine::kind
pub fn file_kind(name: &str) -> (&'static str, Color32) {
    use crate::engine::kind::{Category, category, ext_of};
    match category(name) {
        Category::Archive => ("ZIP", Color32::from_rgb(0xF5, 0x9E, 0x0B)),
        Category::Video => ("MP4", Color32::from_rgb(0x8B, 0x5C, 0xF6)),
        Category::Audio => ("MP3", Color32::from_rgb(0xEC, 0x48, 0x99)),
        Category::Document if ext_of(name) == "pdf" => ("PDF", Color32::from_rgb(0xEF, 0x44, 0x44)),
        Category::Document => ("DOC", Color32::from_rgb(0x3B, 0x82, 0xF6)),
        Category::Program => ("EXE", Color32::from_rgb(0x10, 0xB9, 0x81)),
        Category::DiskImage => ("ISO", Color32::from_rgb(0x64, 0x74, 0x8B)),
        Category::Image => ("IMG", Color32::from_rgb(0x06, 0xB6, 0xD4)),
        Category::Other => ("FILE", Color32::from_rgb(0x94, 0xA3, 0xB8)),
    }
}

/// 文件类型徽标：柔和底色圆角方块 + ASCII 缩写（必然可显示）
pub fn file_badge(ui: &Ui, rect: Rect, name: &str) {
    let (label, c) = file_kind(name);
    let dark = ui.visuals().dark_mode;
    let painter = ui.painter();
    painter.rect_filled(rect, 8, c.gamma_multiply(if dark { 0.22 } else { 0.14 }));
    let size = if label.len() > 3 { rect.width() * 0.26 } else { rect.width() * 0.30 };
    painter.text(rect.center(), Align2::CENTER_CENTER, label, FontId::new(size, theme::bold()), c);
}

/// 圆角细进度条
pub fn progress(ui: &Ui, rect: Rect, frac: f32, color: Color32) {
    let p = theme::pal(ui);
    let r = CornerRadius::same((rect.height() / 2.0) as u8);
    ui.painter().rect_filled(rect, r, p.track);
    let f = frac.clamp(0.0, 1.0);
    if f > 0.0 {
        let mut fr = rect;
        fr.max.x = rect.min.x + (rect.width() * f).max(rect.height());
        ui.painter().rect_filled(fr, r, color);
    }
}

/// IDM 风格分段图：已完成为主色，未完成为轨道色，正在下载的分段前沿高亮
pub fn segment_bar(ui: &mut Ui, t: &Task) {
    let p = theme::pal(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
    let painter = ui.painter().with_clip_rect(rect);
    let r = CornerRadius::same(5);
    let size = t.meta.lock().unwrap().size.unwrap_or(0);
    let st = t.status();
    let done_c = if st == Status::Done { p.success } else { p.accent };
    if size == 0 {
        painter.rect_filled(rect, r, if st == Status::Done { done_c } else { p.track });
        return;
    }
    let segs = t.segs.lock().unwrap();
    if segs.is_empty() {
        drop(segs);
        let f = if st == Status::Done { 1.0 } else { t.downloaded.load(Relaxed) as f32 / size as f32 };
        painter.rect_filled(rect, r, p.track);
        let mut fr = rect;
        fr.max.x = rect.min.x + rect.width() * f.clamp(0.0, 1.0);
        painter.rect_filled(fr, r, done_c);
        return;
    }
    let x = |v: u64| rect.min.x + rect.width() * (v.min(size) as f64 / size as f64) as f32;
    painter.rect_filled(rect, r, done_c);
    for s in segs.iter() {
        let sp = *s.st.lock().unwrap();
        if sp.end > sp.pos {
            let rr = Rect::from_min_max(pos2(x(sp.pos), rect.min.y), pos2(x(sp.end).max(x(sp.pos) + 1.0), rect.max.y));
            painter.rect_filled(rr, 0, p.track);
            if s.busy.load(Relaxed) {
                painter.rect_filled(Rect::from_min_max(pos2(rr.min.x - 2.0, rect.min.y), pos2(rr.min.x, rect.max.y)), 0, p.success);
            }
        }
    }
    // 圆角遮罩：四角用背景色描边掩盖直角
    ui.painter().rect_stroke(rect.expand(0.5), r, Stroke::new(1.5, p.panel), StrokeKind::Outside);
}

/// 速度迷你折线（带渐隐填充）
pub fn sparkline(ui: &Ui, rect: Rect, data: &VecDeque<u64>, color: Color32) {
    let painter = ui.painter();
    let max = data.iter().copied().max().unwrap_or(0).max(1) as f32;
    let n = data.len().max(2);
    let pts: Vec<_> = data
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = rect.max.x - (data.len() - 1 - i) as f32 * rect.width() / (n - 1) as f32;
            pos2(x, rect.max.y - (v as f32 / max) * (rect.height() - 2.0) - 1.0)
        })
        .collect();
    if pts.len() < 2 {
        painter.line_segment([pos2(rect.min.x, rect.max.y - 1.0), pos2(rect.max.x, rect.max.y - 1.0)], Stroke::new(1.0, color.gamma_multiply(0.4)));
        return;
    }
    // 填充：逐列竖条，比非凸多边形更稳定
    let fill = color.gamma_multiply(0.16);
    for w in pts.windows(2) {
        let quad = vec![w[0], w[1], pos2(w[1].x, rect.max.y), pos2(w[0].x, rect.max.y)];
        painter.add(Shape::convex_polygon(quad, fill, Stroke::NONE));
    }
    painter.add(Shape::line(pts, Stroke::new(1.5, color)));
}

/// 应用 Logo：圆角主色方块 + 下载图标
pub fn logo(ui: &Ui, rect: Rect) {
    let p = theme::pal(ui);
    ui.painter().rect_filled(rect, 8, p.accent);
    icons::paint(ui.painter(), rect.shrink(rect.width() * 0.2), Icon::Download, Color32::WHITE);
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, theme::bold())
}

pub fn prop(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
