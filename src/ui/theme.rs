//! 设计令牌、浅色/深色主题、字体与应用图标。
use crate::engine::ThemeMode;
use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Shadow, Stroke, TextStyle, Theme, ThemePreference, Ui, Visuals,
};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Pal {
    pub bg: Color32,
    pub panel: Color32,
    pub side: Color32,
    pub stroke: Color32,
    pub text: Color32,
    pub weak: Color32,
    pub hover: Color32,
    pub track: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub on_accent: Color32,
    pub success: Color32,
    pub danger: Color32,
    pub warning: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub const LIGHT: Pal = Pal {
    bg: hex(0xF7F8FA),
    panel: hex(0xFFFFFF),
    side: hex(0xF0F2F5),
    stroke: hex(0xE5E7EB),
    text: hex(0x1F2937),
    weak: hex(0x6B7280),
    hover: hex(0xEEF0F3),
    track: hex(0xE6E9EE),
    accent: hex(0x2F6FED),
    accent_soft: Color32::from_rgba_premultiplied(5, 11, 24, 26),
    on_accent: Color32::WHITE,
    success: hex(0x16A34A),
    danger: hex(0xDC2626),
    warning: hex(0xD97706),
};

pub const DARK: Pal = Pal {
    bg: hex(0x16181D),
    panel: hex(0x1E2128),
    side: hex(0x1A1D23),
    stroke: hex(0x2A2E37),
    text: hex(0xE5E7EB),
    weak: hex(0x9CA3AF),
    hover: hex(0x272B33),
    track: hex(0x2E333C),
    accent: hex(0x4C8DFF),
    accent_soft: Color32::from_rgba_premultiplied(12, 22, 40, 40),
    on_accent: Color32::WHITE,
    success: hex(0x22C55E),
    danger: hex(0xEF4444),
    warning: hex(0xF59E0B),
};

pub fn pal(ui: &Ui) -> Pal {
    if ui.visuals().dark_mode { DARK } else { LIGHT }
}

pub fn pal_ctx(ctx: &egui::Context) -> Pal {
    if ctx.global_style().visuals.dark_mode { DARK } else { LIGHT }
}

fn visuals(p: &Pal, dark: bool) -> Visuals {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    let r6 = CornerRadius::same(6);
    v.panel_fill = p.bg;
    v.window_fill = p.panel;
    v.faint_bg_color = p.hover;
    v.extreme_bg_color = p.track;
    v.text_edit_bg_color = Some(if dark { hex(0x16181D) } else { hex(0xFFFFFF) });
    v.code_bg_color = p.hover;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    v.weak_text_color = Some(p.weak);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_stroke = Stroke::new(1.0, p.stroke);
    v.window_shadow = Shadow { offset: [0, 10], blur: 32, spread: 0, color: Color32::from_black_alpha(if dark { 110 } else { 38 }) };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(if dark { 90 } else { 28 }) };
    v.selection.bg_fill = p.accent.gamma_multiply(if dark { 0.55 } else { 0.30 });
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.slider_trailing_fill = true;
    v.window_highlight_topmost = false;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.panel;
    w.noninteractive.weak_bg_fill = p.panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.stroke);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r6;

    w.inactive.bg_fill = p.track;
    w.inactive.weak_bg_fill = p.hover;
    w.inactive.bg_stroke = Stroke::new(1.0, p.stroke);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r6;

    w.hovered.bg_fill = p.track;
    w.hovered.weak_bg_fill = p.track;
    w.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.6));
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = r6;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = p.accent;
    w.active.weak_bg_fill = p.track;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.text);
    w.active.corner_radius = r6;
    w.active.expansion = 0.0;

    w.open.bg_fill = p.hover;
    w.open.weak_bg_fill = p.hover;
    w.open.bg_stroke = Stroke::new(1.0, p.stroke);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = r6;
    v
}

pub fn install(ctx: &egui::Context) {
    setup_fonts(ctx);
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, false));
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, true));
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Heading, FontId::proportional(19.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Small, FontId::proportional(12.0)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.menu_margin = egui::Margin::same(6);
        s.spacing.slider_width = 200.0;
    });
}

pub fn apply_mode(ctx: &egui::Context, m: ThemeMode) {
    ctx.set_theme(match m {
        ThemeMode::System => ThemePreference::System,
        ThemeMode::Light => ThemePreference::Light,
        ThemeMode::Dark => ThemePreference::Dark,
    });
}

pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// 拉丁字母用 Segoe UI（专业、数字等宽），中文回退微软雅黑；字体缺失时逐级回退到 egui 内置字体
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let dir = std::path::Path::new("C:\\Windows\\Fonts");
    let mut load = |key: &str, files: &[&str]| -> bool {
        for f in files {
            if let Ok(b) = std::fs::read(dir.join(f)) {
                fonts.font_data.insert(key.into(), Arc::new(egui::FontData::from_owned(b)));
                return true;
            }
        }
        false
    };
    let latin = load("latin", &["segoeui.ttf"]);
    let latin_b = load("latin_b", &["segoeuib.ttf", "seguisb.ttf"]);
    let cjk = load("cjk", &["msyh.ttc", "msyh.ttf", "Deng.ttf", "simhei.ttf", "simsun.ttc"]);

    let builtin = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut prop = Vec::new();
    if latin {
        prop.push("latin".to_string());
    }
    if cjk {
        prop.push("cjk".to_string());
    }
    prop.extend(builtin.iter().cloned());
    let mut bold_fam = Vec::new();
    if latin_b {
        bold_fam.push("latin_b".to_string());
    }
    bold_fam.extend(prop.iter().cloned());
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(bold(), bold_fam);
    if cjk {
        fonts.families.entry(FontFamily::Monospace).or_default().push("cjk".into());
    }
    ctx.set_fonts(fonts);
}

/// 程序化生成窗口/任务栏图标：蓝色圆角方块 + 白色下载箭头（4× 超采样抗锯齿）
pub fn app_icon() -> egui::IconData {
    const N: usize = 64;
    let inside = |x: f32, y: f32| -> Option<bool> {
        // 圆角方块
        let r = 14.0;
        let (cx, cy) = (x.clamp(r + 2.0, N as f32 - r - 2.0), y.clamp(r + 2.0, N as f32 - r - 2.0));
        if (x - cx).powi(2) + (y - cy).powi(2) > r * r {
            return None;
        }
        // 箭头：竖杆 + 三角 + 底横线
        let shaft = (27.5..36.5).contains(&x) && (13.0..33.0).contains(&y);
        let head = (30.0..=46.0).contains(&y) && (x - 32.0).abs() <= 46.0 - y;
        let base = (17.0..47.0).contains(&x) && (49.5..54.0).contains(&y);
        Some(shaft || head || base)
    };
    let mut rgba = vec![0u8; N * N * 4];
    for py in 0..N {
        for px in 0..N {
            let (mut bg, mut fg) = (0u32, 0u32);
            for sy in 0..4 {
                for sx in 0..4 {
                    match inside(px as f32 + (sx as f32 + 0.5) / 4.0, py as f32 + (sy as f32 + 0.5) / 4.0) {
                        Some(true) => fg += 1,
                        Some(false) => bg += 1,
                        None => {}
                    }
                }
            }
            let a = bg + fg;
            if a == 0 {
                continue;
            }
            let t = fg as f32 / a as f32;
            let (r0, g0, b0) = (0x2Fu8 as f32, 0x6Fu8 as f32, 0xEDu8 as f32);
            let i = (py * N + px) * 4;
            rgba[i] = (r0 + (255.0 - r0) * t) as u8;
            rgba[i + 1] = (g0 + (255.0 - g0) * t) as u8;
            rgba[i + 2] = (b0 + (255.0 - b0) * t) as u8;
            rgba[i + 3] = (a * 255 / 16) as u8;
        }
    }
    egui::IconData { rgba, width: N as u32, height: N as u32 }
}
