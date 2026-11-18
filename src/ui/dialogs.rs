//! 对话框：新建下载、设置、删除确认。统一卡片样式。
use super::icons::{self, Icon};
use super::theme::{self, Pal};
use super::widgets::{Btn, bold, button, file_badge, fmt_size, icon_button};
use crate::engine::probe::{self, Info};
use crate::engine::{AddReq, Manager, Settings, ThemeMode};
use eframe::egui::{self, Align, Frame, Id, Key, Layout, Margin, Modal, Rect, RichText, Sense, Stroke, Ui, vec2};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type ProbeSlot = Arc<Mutex<Option<(String, Result<Info, String>)>>>;

pub enum DialogResult {
    None,
    Close,
}

fn modal(ctx: &egui::Context, id: &str, width: f32, add: impl FnOnce(&mut Ui, &Pal)) -> bool {
    let p = theme::pal_ctx(ctx);
    let shadow = ctx.global_style().visuals.window_shadow;
    let frame = Frame::new()
        .fill(p.panel)
        .stroke(Stroke::new(1.0, p.stroke))
        .corner_radius(14)
        .inner_margin(Margin::same(22))
        .shadow(shadow);
    Modal::new(Id::unique(id))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(width);
            add(ui, &p)
        })
        .should_close()
}

/// 标题行：彩色图标 + 标题 + 关闭按钮；返回是否点了关闭
fn header(ui: &mut Ui, p: &Pal, icon: Icon, color: egui::Color32, title: &str) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::hover());
        ui.painter().rect_filled(r, 9, color.gamma_multiply(0.14));
        icons::paint(ui.painter(), r.shrink(9.0), icon, color);
        ui.add_space(4.0);
        ui.label(RichText::new(title).font(bold(17.0)).color(p.text));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            close = icon_button(ui, Icon::X, "关闭（Esc）", true).clicked();
        });
    });
    ui.add_space(14.0);
    close
}

/// 分组卡片
fn section(ui: &mut Ui, p: &Pal, title: &str, add: impl FnOnce(&mut Ui)) {
    ui.label(RichText::new(title).size(12.5).color(p.weak));
    Frame::new()
        .fill(p.bg)
        .stroke(Stroke::new(1.0, p.stroke))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            add(ui)
        });
    ui.add_space(10.0);
}

/// 表单行：左侧固定宽度标签
fn row(ui: &mut Ui, p: &Pal, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized([104.0, 28.0], egui::Label::new(RichText::new(label).color(p.text)));
        add(ui);
    });
}

fn hint(ui: &mut Ui, p: &Pal, text: &str) {
    ui.label(RichText::new(text).size(12.5).color(p.weak));
}

/// 分段选择器
fn segmented<T: PartialEq + Copy>(ui: &mut Ui, p: &Pal, value: &mut T, opts: &[(T, &str)]) {
    let w = 76.0;
    let (outer, _) = ui.allocate_exact_size(vec2(w * opts.len() as f32 + 6.0, 32.0), Sense::hover());
    ui.painter().rect_filled(outer, 8, p.track);
    for (i, (v, label)) in opts.iter().enumerate() {
        let r = Rect::from_min_size(outer.min + vec2(3.0 + w * i as f32, 3.0), vec2(w, 26.0));
        let resp = ui.interact(r, ui.scope_id().with(("seg", i)), Sense::click());
        let on = *value == *v;
        if on {
            ui.painter().rect_filled(r, 6, p.panel);
        } else if resp.hovered() {
            ui.painter().rect_filled(r, 6, p.hover);
        }
        let font = if on { bold(13.0) } else { egui::FontId::proportional(13.0) };
        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, *label, font, if on { p.accent } else { p.text });
        if resp.clicked() {
            *value = *v;
        }
    }
}

/// 底部按钮区（右对齐，先加入的在最右侧）
fn footer(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(8.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        add(ui)
    });
}

// ───────────── 新建下载 ─────────────

pub struct AddDialog {
    urls: String,
    name: String,
    name_edited: bool,
    dir: String,
    conns: u32,
    extra: AddReq,
    probed_url: String,
    probe: ProbeSlot,
    info: Option<Result<Info, String>>,
    focus: bool,
}

impl AddDialog {
    pub fn new(m: &Manager, reqs: Vec<AddReq>) -> Self {
        let s = m.settings.read().unwrap();
        let extra = reqs.first().cloned().unwrap_or_default();
        let urls = reqs.iter().map(|r| r.url.as_str()).collect::<Vec<_>>().join("\n");
        Self {
            name: extra.filename.clone().map(|n| probe::sanitize(&n)).unwrap_or_default(),
            name_edited: extra.filename.is_some(),
            urls,
            dir: s.dir.display().to_string(),
            conns: s.conns,
            extra,
            probed_url: String::new(),
            probe: Arc::default(),
            info: None,
            focus: true,
        }
    }

    pub fn append(&mut self, reqs: Vec<AddReq>) {
        for r in reqs {
            if !self.urls.lines().any(|l| l.trim() == r.url) {
                if !self.urls.trim().is_empty() {
                    self.urls.push('\n');
                }
                self.urls.push_str(&r.url);
            }
        }
    }

    fn url_list(&self) -> Vec<String> {
        self.urls
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
            .map(String::from)
            .collect()
    }

    /// 单链接时后台探测文件名和大小
    fn auto_probe(&mut self, m: &Arc<Manager>) {
        let list = self.url_list();
        let url = if list.len() == 1 { list[0].clone() } else { String::new() };
        if url != self.probed_url {
            self.probed_url = url.clone();
            self.info = None;
            if !url.is_empty() {
                let slot = self.probe.clone();
                let client = m.client();
                let mut req = self.extra.clone();
                req.url = url.clone();
                let mm = m.clone();
                m.rt.spawn(async move {
                    let r = probe::probe(&client, &req).await;
                    *slot.lock().unwrap() = Some((url, r));
                    mm.repaint();
                });
            }
        }
        if let Some((u, r)) = self.probe.lock().unwrap().take()
            && u == self.probed_url
        {
            if let Ok(i) = &r
                && !self.name_edited
            {
                self.name = i.filename.clone();
            }
            self.info = Some(r);
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, m: &Arc<Manager>) -> DialogResult {
        self.auto_probe(m);
        let mut result = DialogResult::None;
        let outside = modal(ctx, "add_dialog", 580.0, |ui, p| {
            if header(ui, p, Icon::Download, p.accent, "新建下载") {
                result = DialogResult::Close;
            }
            ui.label(RichText::new("下载链接").color(p.text));
            let te = ui.add(
                egui::TextEdit::multiline(&mut self.urls)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 8))
                    .hint_text("粘贴下载链接，每行一个"),
            );
            if self.focus {
                te.request_focus();
                self.focus = false;
            }
            ui.add_space(10.0);
            let n = self.url_list().len();
            // 文件信息预览卡
            if n >= 1 {
                Frame::new()
                    .fill(p.bg)
                    .stroke(Stroke::new(1.0, p.stroke))
                    .corner_radius(10)
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if n == 1 {
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(vec2(42.0, 42.0), Sense::hover());
                                let shown = if self.name.is_empty() { probe::url_name(&self.probed_url).unwrap_or_default() } else { self.name.clone() };
                                file_badge(ui, r, &shown);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 4.0;
                                    if ui
                                        .add(
                                            egui::TextEdit::singleline(&mut self.name)
                                                .desired_width(ui.available_width())
                                                .hint_text("文件名（自动识别）"),
                                        )
                                        .changed()
                                    {
                                        self.name_edited = true;
                                    }
                                    ui.horizontal(|ui| match &self.info {
                                        None => {
                                            ui.add(egui::Spinner::new().size(12.0));
                                            ui.label(RichText::new("正在获取文件信息").size(12.5).color(p.weak));
                                        }
                                        Some(Ok(i)) => {
                                            let sz = i.size.map(fmt_size).unwrap_or_else(|| "大小未知".into());
                                            ui.label(RichText::new(sz).size(12.5).color(p.text));
                                            let (t, c) = if i.resumable { ("支持断点续传", p.success) } else { ("不支持断点续传", p.warning) };
                                            ui.label(RichText::new(t).size(12.5).color(c));
                                        }
                                        Some(Err(e)) => {
                                            ui.add(
                                                egui::Label::new(RichText::new(format!("获取失败：{e}")).size(12.5).color(p.danger))
                                                    .truncate(),
                                            );
                                        }
                                    });
                                });
                            });
                        } else {
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                                icons::paint(ui.painter(), r, Icon::Copy, p.accent);
                                ui.label(RichText::new(format!("共 {n} 个链接，将按服务器提供的文件名保存")).color(p.text));
                            });
                        }
                    });
                ui.add_space(10.0);
            }
            row(ui, p, "保存到", |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.dir).desired_width(ui.available_width() - 40.0));
                if icon_button(ui, Icon::Folder, "选择文件夹", true).clicked()
                    && let Some(d) = rfd::FileDialog::new().set_directory(&self.dir).pick_folder()
                {
                    self.dir = d.display().to_string();
                }
            });
            row(ui, p, "连接数", |ui| {
                ui.add(egui::Slider::new(&mut self.conns, 1..=32));
            });
            ui.add_space(6.0);
            let mut go = None;
            footer(ui, |ui| {
                if ui.add_enabled_ui(n > 0, |ui| button(ui, Btn::Primary, Some(Icon::Download), "开始下载")).inner.clicked() {
                    go = Some(true);
                }
                if ui.add_enabled_ui(n > 0, |ui| button(ui, Btn::Secondary, Some(Icon::Clock), "稍后下载")).inner.clicked() {
                    go = Some(false);
                }
                if button(ui, Btn::Secondary, None, "取消").clicked() {
                    result = DialogResult::Close;
                }
                ui.label(RichText::new("Ctrl+Enter 开始").size(12.0).color(p.weak));
            });
            // 多行输入框内回车用于换行，Ctrl+Enter 或焦点不在输入框时回车开始
            if n > 0 && ui.input(|i| i.key_pressed(Key::Enter) && (i.modifiers.command || !te.has_focus())) {
                go = Some(true);
            }
            if let Some(start) = go {
                let dir = PathBuf::from(self.dir.trim());
                let urls = self.url_list();
                let single = urls.len() == 1;
                for u in urls {
                    let mut req = self.extra.clone();
                    req.url = u;
                    let name = if single { self.name.as_str() } else { "" };
                    m.add(req, name, dir.clone(), self.conns, start);
                }
                result = DialogResult::Close;
            }
        });
        if outside {
            result = DialogResult::Close;
        }
        result
    }
}

// ───────────── 设置 ─────────────

pub struct SettingsDialog {
    s: Settings,
    cleaned: bool,
}

impl SettingsDialog {
    pub fn new(m: &Manager) -> Self {
        Self { s: m.settings.read().unwrap().clone(), cleaned: false }
    }

    pub fn show(&mut self, ctx: &egui::Context, m: &Arc<Manager>) -> DialogResult {
        let mut result = DialogResult::None;
        let max_h = (ctx.content_rect().height() - 200.0).clamp(240.0, 560.0);
        let outside = modal(ctx, "settings", 560.0, |ui, p| {
            if header(ui, p, Icon::Settings, p.accent, "设置") {
                result = DialogResult::Close;
            }
            let s = &mut self.s;
            egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                section(ui, p, "常规", |ui| {
                    row(ui, p, "默认保存目录", |ui| {
                        let mut d = s.dir.display().to_string();
                        if ui.add(egui::TextEdit::singleline(&mut d).desired_width(ui.available_width() - 40.0)).changed() {
                            s.dir = d.into();
                        }
                        if icon_button(ui, Icon::Folder, "选择文件夹", true).clicked()
                            && let Some(d) = rfd::FileDialog::new().set_directory(&s.dir).pick_folder()
                        {
                            s.dir = d;
                        }
                    });
                    row(ui, p, "外观", |ui| {
                        segmented(ui, p, &mut s.theme, &[(ThemeMode::System, "跟随系统"), (ThemeMode::Light, "浅色"), (ThemeMode::Dark, "深色")]);
                    });
                });
                section(ui, p, "下载", |ui| {
                    row(ui, p, "每任务连接数", |ui| {
                        ui.add(egui::Slider::new(&mut s.conns, 1..=32));
                    });
                    row(ui, p, "同时下载任务", |ui| {
                        ui.add(egui::Slider::new(&mut s.max_active, 1..=10));
                    });
                    row(ui, p, "全局限速", |ui| {
                        ui.add(egui::DragValue::new(&mut s.speed_limit_kb).speed(64).range(0..=10_000_000).suffix(" KB/s"));
                        hint(ui, p, "0 表示不限速");
                    });
                });
                section(ui, p, "网络", |ui| {
                    row(ui, p, "代理", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut s.proxy)
                                .desired_width(ui.available_width())
                                .hint_text("http://127.0.0.1:10808"),
                        );
                    });
                    hint(ui, p, "留空使用系统代理，填 direct 直连，支持 http:// 与 socks5://");
                });
                section(ui, p, "剪贴板与浏览器", |ui| {
                    row(ui, p, "剪贴板监听", |ui| {
                        ui.checkbox(&mut s.clipboard, "复制下载链接时自动弹出新建窗口");
                    });
                    row(ui, p, "监听扩展名", |ui| {
                        ui.add(egui::TextEdit::multiline(&mut s.exts).desired_rows(2).desired_width(ui.available_width()));
                    });
                    row(ui, p, "扩展端口", |ui| {
                        ui.add(egui::DragValue::new(&mut s.port).range(1024..=65535));
                        hint(ui, p, "重启后生效");
                    });
                });
                section(ui, p, "存储与缓存", |ui| {
                    row(ui, p, "自动清理记录", |ui| {
                        ui.add(egui::DragValue::new(&mut s.clean_done_days).range(0..=3650).suffix(" 天"));
                        hint(ui, p, "已完成记录保留天数，0 为永久保留");
                    });
                    row(ui, p, "临时文件", |ui| {
                        if button(ui, Btn::Secondary, Some(Icon::Trash), "立即清理缓存").clicked() {
                            let mm = m.clone();
                            m.rt.spawn_blocking(move || mm.cleanup());
                            self.cleaned = true;
                        }
                        if self.cleaned {
                            ui.label(RichText::new("已清理").size(12.5).color(p.success));
                        }
                    });
                });
            });
            footer(ui, |ui| {
                if button(ui, Btn::Primary, Some(Icon::Check), "保存").clicked() {
                    m.apply_settings(self.s.clone());
                    result = DialogResult::Close;
                }
                if button(ui, Btn::Secondary, None, "取消").clicked() {
                    result = DialogResult::Close;
                }
            });
        });
        if outside {
            result = DialogResult::Close;
        }
        result
    }
}

// ───────────── 删除确认 ─────────────

pub struct DeleteDialog {
    pub ids: Vec<u64>,
    delete_file: bool,
}

impl DeleteDialog {
    pub fn new(ids: Vec<u64>) -> Self {
        Self { ids, delete_file: false }
    }

    pub fn show(&mut self, ctx: &egui::Context, m: &Arc<Manager>) -> DialogResult {
        let mut result = DialogResult::None;
        let outside = modal(ctx, "delete", 380.0, |ui, p| {
            if header(ui, p, Icon::Trash, p.danger, "删除任务") {
                result = DialogResult::Close;
            }
            ui.label(RichText::new(format!("确定删除选中的 {} 个任务吗？", self.ids.len())).color(p.text));
            ui.add_space(4.0);
            ui.checkbox(&mut self.delete_file, "同时删除已下载的文件");
            ui.add_space(6.0);
            footer(ui, |ui| {
                if button(ui, Btn::Danger, Some(Icon::Trash), "删除").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    m.remove(&self.ids, self.delete_file);
                    result = DialogResult::Close;
                }
                if button(ui, Btn::Secondary, None, "取消").clicked() {
                    result = DialogResult::Close;
                }
            });
        });
        if outside {
            result = DialogResult::Close;
        }
        result
    }
}
