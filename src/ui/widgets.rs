use crate::engine::Task;
use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Ui, Vec2, pos2};
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
    if b == 0 { String::new() } else { format!("{}/s", fmt_size(b)) }
}

pub fn fmt_eta(secs: u64) -> String {
    match secs {
        0 => String::new(),
        s if s < 60 => format!("{s}秒"),
        s if s < 3600 => format!("{}分{:02}秒", s / 60, s % 60),
        s if s < 86400 * 3 => format!("{}时{:02}分", s / 3600, s % 3600 / 60),
        _ => "∞".into(),
    }
}

pub const ACCENT: Color32 = Color32::from_rgb(0x2f, 0x80, 0xed);
pub const GREEN: Color32 = Color32::from_rgb(0x27, 0xae, 0x60);
pub const RED: Color32 = Color32::from_rgb(0xeb, 0x57, 0x57);
pub const GRAY: Color32 = Color32::from_rgb(0x9a, 0xa0, 0xa6);

/// 简洁进度条
pub fn progress(ui: &Ui, rect: Rect, frac: f32, color: Color32) {
    let p = ui.painter();
    let r = CornerRadius::same(3);
    p.rect_filled(rect, r, ui.visuals().extreme_bg_color);
    if frac > 0.0 {
        let mut f = rect;
        f.max.x = rect.min.x + rect.width() * frac.clamp(0.0, 1.0);
        p.rect_filled(f, r, color);
    }
}

/// IDM 风格分段图：浅色为整体，深色为每段已完成部分，竖线为段边界
pub fn segment_bar(ui: &mut Ui, t: &Task) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), ui.visuals().extreme_bg_color);
    let size = t.meta.lock().unwrap().size.unwrap_or(0);
    if size == 0 {
        if t.status() == crate::engine::Status::Done {
            p.rect_filled(rect, CornerRadius::same(2), GREEN);
        }
        return;
    }
    let segs = t.segs.lock().unwrap();
    if segs.is_empty() {
        let f = t.downloaded.load(Relaxed) as f32 / size as f32;
        progress(ui, rect, f, if t.status() == crate::engine::Status::Done { GREEN } else { ACCENT });
        return;
    }
    let x = |v: u64| rect.min.x + rect.width() * (v as f64 / size as f64) as f32;
    // 已完成区 = 全部区间减去各段剩余区间
    p.rect_filled(rect, CornerRadius::same(2), ACCENT);
    for s in segs.iter() {
        let st = *s.st.lock().unwrap();
        if st.end > st.pos {
            let r = Rect::from_min_max(pos2(x(st.pos), rect.min.y), pos2(x(st.end.min(size)), rect.max.y));
            p.rect_filled(r, CornerRadius::ZERO, ui.visuals().extreme_bg_color);
            if s.busy.load(Relaxed) {
                p.vline(r.min.x, rect.y_range(), egui::Stroke::new(2.0, GREEN));
            }
        }
    }
}
