//! 对话框：新建下载、设置、删除确认、更新链接、扩展安装向导、关闭询问、完成动作倒计时。统一卡片样式。
use super::icons::{self, Icon};
use super::theme::{self, Pal};
use super::widgets::{Btn, bold, button, file_badge, fmt_size, icon_button};
use crate::browser::{self, Browser};
use crate::engine::probe::{self, Info};
use crate::engine::{AddReq, Manager, NewTask, Settings, StartMode, Status, ThemeMode, sched};
use eframe::egui::{
    self, Align, Align2, Color32, Frame, Id, Key, Layout, Margin, Modal, Rect, RichText, Sense, Stroke, Ui, vec2,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type ProbeSlot = Arc<Mutex<Option<(String, Result<Info, String>)>>>;

pub enum DialogResult {
    None,
    Close,
    /// 从设置中打开扩展安装向导
    OpenExtGuide,
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
fn header(ui: &mut Ui, p: &Pal, icon: Icon, color: Color32, title: &str) -> bool {
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

fn status_line(ui: &mut Ui, icon: Icon, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
        icons::paint(ui.painter(), r, icon, color);
        ui.label(RichText::new(text).size(13.0).color(color));
    });
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
        ui.painter().text(r.center(), Align2::CENTER_CENTER, *label, font, if on { p.accent } else { p.text });
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

/// 带编号圆点的步骤说明
fn step(ui: &mut Ui, p: &Pal, n: u32, text: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 11.0, p.accent_soft);
        ui.painter().text(r.center(), Align2::CENTER_CENTER, n.to_string(), bold(12.0), p.accent);
        ui.add(egui::Label::new(RichText::new(text).color(p.text)).wrap());
    });
}

// ───────────── 新建下载 ─────────────

pub struct AddDialog {
    urls: String,
    name: String,
    name_edited: bool,
    dir: String,
    conns: u32,
    sha256: String,
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
            sha256: String::new(),
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
        let outside = modal(ctx, "add_dialog", 600.0, |ui, p| {
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
            let list = self.url_list();
            let n = list.len();
            let dups = list.iter().filter_map(|u| m.find_url(u)).collect::<Vec<_>>();
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
                                let shown = if self.name.is_empty() {
                                    probe::url_name(&self.probed_url).unwrap_or_default()
                                } else {
                                    self.name.clone()
                                };
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
                                            let (t, c) =
                                                if i.resumable { ("支持断点续传", p.success) } else { ("不支持断点续传", p.warning) };
                                            ui.label(RichText::new(t).size(12.5).color(c));
                                        }
                                        Some(Err(e)) => {
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(format!("获取失败：{e}")).size(12.5).color(p.danger),
                                                )
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
                        if !dups.is_empty() {
                            ui.add_space(6.0);
                            let txt = if n == 1 {
                                format!("该链接已在下载列表中（{}），仍可再次下载", dups[0].label())
                            } else {
                                format!("其中 {} 个链接已在下载列表中，仍可再次下载", dups.len())
                            };
                            status_line(ui, Icon::Alert, p.warning, &txt);
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
            row(ui, p, "SHA-256", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.sha256)
                        .desired_width(ui.available_width())
                        .hint_text("可选，填写后完成时自动校验"),
                );
            });
            ui.add_space(6.0);
            let mut go = None;
            footer(ui, |ui| {
                let ok = n > 0;
                if ui
                    .add_enabled_ui(ok, |ui| button(ui, Btn::Primary, Some(Icon::Download), "开始下载"))
                    .inner
                    .on_hover_text("Ctrl+Enter")
                    .clicked()
                {
                    go = Some(StartMode::Now);
                }
                if ui
                    .add_enabled_ui(ok, |ui| button(ui, Btn::Secondary, Some(Icon::Clock), "计划下载"))
                    .inner
                    .on_hover_text("按设置中的计划时间自动开始")
                    .clicked()
                {
                    go = Some(StartMode::Scheduled);
                }
                if ui.add_enabled_ui(ok, |ui| button(ui, Btn::Secondary, None, "稍后下载")).inner.clicked() {
                    go = Some(StartMode::Later);
                }
                if button(ui, Btn::Secondary, None, "取消").clicked() {
                    result = DialogResult::Close;
                }
            });
            // 多行输入框内回车用于换行，Ctrl+Enter 或焦点不在输入框时回车开始
            if n > 0 && ui.input(|i| i.key_pressed(Key::Enter) && (i.modifiers.command || !te.has_focus())) {
                go = Some(StartMode::Now);
            }
            if let Some(mode) = go {
                let default_dir = m.settings.read().unwrap().dir.clone();
                let dir = PathBuf::from(self.dir.trim());
                let auto_dir = dir == default_dir;
                let single = list.len() == 1;
                let sha = Some(self.sha256.trim().to_string()).filter(|s| single && !s.is_empty());
                for u in list {
                    let mut req = self.extra.clone();
                    req.url = u;
                    m.add(NewTask {
                        req,
                        name: if single { self.name.clone() } else { String::new() },
                        dir: dir.clone(),
                        auto_dir,
                        conns: self.conns,
                        mode,
                        sha256: sha.clone(),
                    });
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
        let max_h = (ctx.content_rect().height() - 200.0).clamp(240.0, 600.0);
        let outside = modal(ctx, "settings", 580.0, |ui, p| {
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
                    row(ui, p, "按类型分类", |ui| {
                        ui.checkbox(&mut s.categorize, "存入默认目录下的 压缩文件、视频、音乐、文档、程序 等子目录");
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
                    row(ui, p, "出错自动重试", |ui| {
                        ui.add(egui::DragValue::new(&mut s.auto_retry).range(0..=20).suffix(" 次"));
                        hint(ui, p, "每次间隔 30 秒，0 表示不重试");
                    });
                });
                section(ui, p, "计划任务", |ui| {
                    row(ui, p, "启用计划", |ui| {
                        ui.checkbox(&mut s.schedule.enabled, "每天按时自动开始“计划下载”的任务");
                    });
                    row(ui, p, "开始时间", |ui| {
                        ui.add(egui::TextEdit::singleline(&mut s.schedule.start).desired_width(70.0).hint_text("02:00"));
                        ui.label(RichText::new("停止时间").color(p.text));
                        ui.add(egui::TextEdit::singleline(&mut s.schedule.stop).desired_width(70.0).hint_text("可留空"));
                    });
                    let bad_start = sched::parse_hm(&s.schedule.start).is_none();
                    let bad_stop = !s.schedule.stop.trim().is_empty() && sched::parse_hm(&s.schedule.stop).is_none();
                    if s.schedule.enabled && (bad_start || bad_stop) {
                        status_line(ui, Icon::Alert, p.danger, "时间格式应为 HH:MM，例如 02:30");
                    } else {
                        hint(ui, p, "停止时间留空表示不自动暂停；开始晚于停止表示跨午夜");
                    }
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
                section(ui, p, "托盘与启动", |ui| {
                    row(ui, p, "关闭窗口", |ui| {
                        ui.checkbox(&mut s.close_to_tray, "最小化到系统托盘，后台继续下载");
                    });
                    row(ui, p, "开机自启动", |ui| {
                        ui.checkbox(&mut s.autostart, "登录 Windows 后自动启动并最小化到托盘");
                    });
                });
                section(ui, p, "剪贴板与浏览器", |ui| {
                    row(ui, p, "浏览器扩展", |ui| {
                        if browser::connected() {
                            status_line(ui, Icon::Check, p.success, "已连接");
                        } else {
                            status_line(ui, Icon::Alert, p.weak, "本次运行尚未检测到");
                        }
                        if button(ui, Btn::Secondary, Some(Icon::External), "安装向导").clicked() {
                            result = DialogResult::OpenExtGuide;
                        }
                    });
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
                    let old_auto = m.settings.read().unwrap().autostart;
                    if self.s.autostart != old_auto && !crate::tray::set_autostart(self.s.autostart) {
                        self.s.autostart = old_auto;
                    }
                    m.apply_settings(self.s.clone());
                    result = DialogResult::Close;
                }
                if button(ui, Btn::Secondary, None, "取消").clicked() {
                    result = DialogResult::Close;
                }
            });
        });
        if outside && matches!(result, DialogResult::None) {
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

// ───────────── 更新下载链接 ─────────────

pub struct RelinkDialog {
    id: u64,
    name: String,
    url: String,
    pending: bool,
    slot: Arc<Mutex<Option<Result<(), String>>>>,
    err: Option<String>,
}

impl RelinkDialog {
    pub fn new(m: &Manager, id: u64) -> Option<Self> {
        let t = m.get(id)?;
        let name = t.meta.lock().unwrap().name.clone();
        Some(Self { id, name, url: t.url(), pending: false, slot: Arc::default(), err: None })
    }

    pub fn show(&mut self, ctx: &egui::Context, m: &Arc<Manager>) -> DialogResult {
        if let Some(r) = self.slot.lock().unwrap().take() {
            self.pending = false;
            match r {
                Ok(()) => return DialogResult::Close,
                Err(e) => self.err = Some(e),
            }
        }
        let mut result = DialogResult::None;
        let outside = modal(ctx, "relink", 560.0, |ui, p| {
            if header(ui, p, Icon::Refresh, p.accent, "更新下载链接") {
                result = DialogResult::Close;
            }
            ui.add(egui::Label::new(RichText::new(&self.name).font(bold(14.0)).color(p.text)).truncate());
            hint(ui, p, "链接过期或失效时，粘贴同一文件的新链接即可从断点继续下载");
            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::multiline(&mut self.url)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 8)),
            );
            if self.pending {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(RichText::new("正在验证新链接").size(13.0).color(p.weak));
                });
            } else if let Some(e) = &self.err {
                status_line(ui, Icon::Alert, p.danger, e);
            }
            footer(ui, |ui| {
                let ok = !self.pending && self.url.trim().starts_with("http");
                if ui.add_enabled_ui(ok, |ui| button(ui, Btn::Primary, Some(Icon::Refresh), "更新并继续")).inner.clicked() {
                    self.pending = true;
                    self.err = None;
                    let (mm, id, url, slot) = (m.clone(), self.id, self.url.trim().to_string(), self.slot.clone());
                    m.rt.spawn(async move {
                        let r = mm.relink(id, url).await;
                        *slot.lock().unwrap() = Some(r);
                        mm.repaint();
                    });
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

// ───────────── 浏览器扩展安装向导 ─────────────

pub struct ExtGuide {
    browsers: Vec<(Browser, PathBuf)>,
    launched: Option<Browser>,
    err: Option<String>,
}

impl ExtGuide {
    pub fn new() -> Self {
        Self { browsers: browser::detect(), launched: None, err: None }
    }

    /// 返回 Close 时若 done 为 true，表示以后不再自动弹出
    pub fn show(&mut self, ctx: &egui::Context, m: &Arc<Manager>) -> DialogResult {
        let mut result = DialogResult::None;
        let mut done = false;
        let connected = browser::connected();
        let outside = modal(ctx, "ext_guide", 560.0, |ui, p| {
            if header(ui, p, Icon::External, p.accent, "安装浏览器扩展") {
                result = DialogResult::Close;
            }
            ui.label(RichText::new("安装后，浏览器中的下载会自动交给 SwiftDL 多线程加速，并保留登录状态。").color(p.text));
            hint(ui, p, "Chrome 与 Edge 只允许用户本人加载商店之外的扩展，下面两步即可完成。");
            ui.add_space(10.0);
            if connected {
                status_line(ui, Icon::Check, p.success, "扩展已连接，安装完成");
            } else if self.browsers.is_empty() {
                status_line(ui, Icon::Alert, p.warning, "未检测到 Chrome 或 Edge，可手动加载扩展文件夹");
                ui.add_space(6.0);
                if button(ui, Btn::Primary, Some(Icon::Folder), "打开扩展文件夹").clicked() {
                    match browser::extract() {
                        Ok(dir) => {
                            ctx.copy_text(dir.display().to_string());
                            let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                        }
                        Err(e) => self.err = Some(format!("释放扩展失败：{e}")),
                    }
                }
            } else {
                ui.horizontal(|ui| {
                    for (i, (b, exe)) in self.browsers.clone().into_iter().enumerate() {
                        let kind = if i == 0 { Btn::Primary } else { Btn::Secondary };
                        let label = format!("在 {} 中安装", if b == Browser::Edge { "Edge" } else { "Chrome" });
                        if button(ui, kind, Some(Icon::External), &label).on_hover_text(b.name()).clicked() {
                            match browser::launch(&exe, b) {
                                Ok(dir) => {
                                    ctx.copy_text(dir.display().to_string());
                                    self.launched = Some(b);
                                    self.err = None;
                                }
                                Err(e) => self.err = Some(format!("启动浏览器失败：{e}")),
                            }
                        }
                    }
                });
            }
            if let Some(e) = &self.err {
                status_line(ui, Icon::Alert, p.danger, e);
            }
            if !connected && (self.launched.is_some() || self.browsers.is_empty()) {
                ui.add_space(12.0);
                Frame::new()
                    .fill(p.bg)
                    .stroke(Stroke::new(1.0, p.stroke))
                    .corner_radius(10)
                    .inner_margin(Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 10.0;
                        step(ui, p, 1, "在打开的扩展页面中，开启右上角（Edge 为左下角）的“开发者模式”");
                        step(ui, p, 2, "把资源管理器中已选中的 extension 文件夹拖到扩展页面上；或点击“加载解压缩的扩展”，按 Ctrl+V 粘贴已复制的路径");
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(14.0));
                            ui.label(RichText::new("等待扩展连接，安装完成后会自动提示").size(13.0).color(p.weak));
                        });
                    });
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            }
            footer(ui, |ui| {
                if connected {
                    if button(ui, Btn::Primary, Some(Icon::Check), "完成").clicked() {
                        done = true;
                    }
                } else {
                    if button(ui, Btn::Secondary, None, "稍后").clicked() {
                        result = DialogResult::Close;
                    }
                    if button(ui, Btn::Secondary, None, "已安装，不再提示").clicked() {
                        done = true;
                    }
                }
            });
        });
        if done {
            let mut s = m.settings.read().unwrap().clone();
            s.ext_guide_done = true;
            m.apply_settings(s);
            result = DialogResult::Close;
        }
        if outside {
            result = DialogResult::Close;
        }
        result
    }
}

// ───────────── 关闭询问 ─────────────

pub enum CloseChoice {
    None,
    Cancel,
    Tray { remember: bool },
    Quit { remember: bool },
}

pub struct CloseAsk {
    remember: bool,
}

impl CloseAsk {
    pub fn new() -> Self {
        Self { remember: true }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> CloseChoice {
        let mut choice = CloseChoice::None;
        let outside = modal(ctx, "close_ask", 400.0, |ui, p| {
            if header(ui, p, Icon::Download, p.accent, "关闭 SwiftDL") {
                choice = CloseChoice::Cancel;
            }
            ui.label(RichText::new("关闭窗口后，是否在系统托盘中继续下载？").color(p.text));
            ui.add_space(4.0);
            ui.checkbox(&mut self.remember, "记住我的选择（可在设置中修改）");
            footer(ui, |ui| {
                if button(ui, Btn::Primary, None, "最小化到托盘").clicked() {
                    choice = CloseChoice::Tray { remember: self.remember };
                }
                if button(ui, Btn::Secondary, None, "退出程序").clicked() {
                    choice = CloseChoice::Quit { remember: self.remember };
                }
            });
        });
        if outside && matches!(choice, CloseChoice::None) {
            choice = CloseChoice::Cancel;
        }
        choice
    }
}

// ───────────── 完成动作倒计时 ─────────────

/// 返回 Some(true) 立即执行，Some(false) 取消
pub fn countdown(ctx: &egui::Context, action: &str, remaining: u64) -> Option<bool> {
    let mut r = None;
    let outside = modal(ctx, "countdown", 380.0, |ui, p| {
        if header(ui, p, Icon::Clock, p.warning, "全部下载已完成") {
            r = Some(false);
        }
        ui.label(RichText::new(format!("{remaining} 秒后将{action}")).font(bold(16.0)).color(p.text));
        let frac = remaining as f32 / 60.0;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
        ui.painter().rect_filled(rect, 3, p.track);
        let mut f = rect;
        f.max.x = rect.min.x + rect.width() * frac;
        ui.painter().rect_filled(f, 3, p.warning);
        footer(ui, |ui| {
            if button(ui, Btn::Primary, None, "立即执行").clicked() {
                r = Some(true);
            }
            if button(ui, Btn::Secondary, None, "取消").clicked() {
                r = Some(false);
            }
        });
    });
    if outside && r.is_none() {
        r = Some(false);
    }
    r
}

/// 状态标签：计划中的暂停任务显示为“已计划”
pub fn status_text(s: Status, scheduled: bool) -> &'static str {
    if s == Status::Paused && scheduled { "已计划" } else { s.label() }
}
