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
    /// 立即检查更新
    CheckUpdate,
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
            close = icon_button(ui, Icon::X, tr!("关闭（Esc）"), true).clicked();
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

fn is_link(l: &str) -> bool {
    let low = l.to_ascii_lowercase();
    ["http://", "https://", "ftp://", "ftps://", "sftp://"].iter().any(|p| low.starts_with(p)) || crate::engine::bt::is_bt(l)
}

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
    /// HLS 主列表中选中的清晰度序号（0 = 最高）
    variant: usize,
    /// FTP/SFTP 登录信息（只在单个远程链接时显示）
    user: String,
    pass: String,
    key: String,
    cred_url: String,
    probed_key: String,
    /// BT：每个文件是否下载
    bt_sel: Vec<bool>,
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
            variant: 0,
            user: String::new(),
            pass: String::new(),
            key: String::new(),
            cred_url: String::new(),
            probed_key: String::new(),
            bt_sel: Vec::new(),
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
            .filter(|l| is_link(l))
            .map(String::from)
            .collect()
    }

    /// 单个 FTP/SFTP 链接：返回带账号密码的实际链接（新建任务时密码会被加密移出链接）
    fn with_creds(&self, url: &str) -> String {
        match url::Url::parse(url) {
            Ok(mut u) if crate::engine::remote::is_remote(url) => {
                if !self.user.trim().is_empty() {
                    let _ = u.set_username(self.user.trim());
                }
                if !self.pass.is_empty() {
                    let _ = u.set_password(Some(&self.pass));
                }
                u.to_string()
            }
            _ => url.to_string(),
        }
    }

    /// 首次看到某个远程链接时，从链接中带出用户名和密码
    fn prefill_creds(&mut self, url: &str) {
        if url == self.cred_url || !crate::engine::remote::is_remote(url) {
            return;
        }
        self.cred_url = url.to_string();
        if let Ok(u) = url::Url::parse(url) {
            fn dec(s: &str) -> String {
                percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
            }
            self.user = dec(u.username());
            self.pass = u.password().map(dec).unwrap_or_default();
        }
    }

    /// 单链接时后台探测文件名和大小
    fn auto_probe(&mut self, m: &Arc<Manager>) {
        let list = self.url_list();
        let url = if list.len() == 1 { list[0].clone() } else { String::new() };
        self.prefill_creds(&url);
        let eff = self.with_creds(&url);
        let key = format!("{eff}|{}", self.key);
        if key != self.probed_key {
            self.probed_key = key.clone();
            self.probed_url = url.clone();
            self.info = None;
            self.variant = 0;
            if !url.is_empty() {
                let slot = self.probe.clone();
                let client = m.client_for(&eff);
                let mut req = self.extra.clone();
                req.url = eff;
                req.key_path = Some(self.key.clone()).filter(|k| !k.trim().is_empty());
                let url = key;
                let mm = m.clone();
                m.rt.spawn(async move {
                    let r = if crate::engine::bt::is_bt(&req.url) {
                        crate::engine::bt::probe(&mm, &client, &req).await
                    } else {
                        probe::probe(&client, &req).await
                    };
                    *slot.lock().unwrap() = Some((url, r));
                    mm.repaint();
                });
            }
        }
        if let Some((u, r)) = self.probe.lock().unwrap().take()
            && u == self.probed_key
        {
            if let Ok(i) = &r
                && !self.name_edited
            {
                self.name = i.filename.clone();
            }
            if let Ok(i) = &r {
                self.bt_sel = i.bt.as_ref().map(|b| vec![true; b.files.len()]).unwrap_or_default();
            }
            self.info = Some(r);
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, m: &Arc<Manager>) -> DialogResult {
        self.auto_probe(m);
        let mut result = DialogResult::None;
        let outside = modal(ctx, "add_dialog", 600.0, |ui, p| {
            if header(ui, p, Icon::Download, p.accent, tr!("新建下载")) {
                result = DialogResult::Close;
            }
            ui.label(RichText::new(tr!("下载链接")).color(p.text));
            let te = ui.add(
                egui::TextEdit::multiline(&mut self.urls)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 8))
                    .hint_text(tr!("粘贴下载链接，每行一个")),
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
                                                .hint_text(tr!("文件名（自动识别）")),
                                        )
                                        .changed()
                                    {
                                        self.name_edited = true;
                                    }
                                    ui.horizontal(|ui| match &self.info {
                                        None => {
                                            ui.add(egui::Spinner::new().size(12.0));
                                            let msg = if crate::engine::bt::is_bt(&self.probed_url) {
                                                tr!("正在获取种子信息（磁力链接需要从 DHT 网络查找节点，可能需要一些时间）")
                                            } else {
                                                tr!("正在获取文件信息")
                                            };
                                            ui.label(RichText::new(msg).size(12.5).color(p.weak));
                                        }
                                        Some(Ok(i)) if i.bt.is_some() => {
                                            let b = i.bt.as_ref().unwrap();
                                            let sel: u64 = b.files.iter().zip(&self.bt_sel).filter(|(_, s)| **s).map(|(f, _)| f.1).sum();
                                            ui.label(
                                                RichText::new(trf!("BT 任务  {} 个文件  已选 {} / 共 {}", b.files.len(), fmt_size(sel), fmt_size(i.size.unwrap_or(0))))
                                                    .size(12.5)
                                                    .color(p.text),
                                            )
                                            .on_hover_text(format!("{}
info hash: {}", b.name, b.info_hash));
                                        }
                                        Some(Ok(i)) if i.hls.is_some() => {
                                            let h = i.hls.as_ref().unwrap();
                                            let mins = h.duration as u64 / 60;
                                            let mut t = trf!("HLS 视频流  {} 段  时长 {}:{}", h.segments, mins, format!("{:02}", h.duration as u64 % 60));
                                            if h.encrypted {
                                                t.push_str(tr!("  已加密"));
                                            }
                                            ui.label(RichText::new(t).size(12.5).color(p.text));
                                            if h.variants.len() > 1 {
                                                let cur = h.variants.get(self.variant).map(|v| v.label()).unwrap_or_default();
                                                egui::ComboBox::from_id_salt("hls_variant").selected_text(cur).show_ui(ui, |ui| {
                                                    for (k, v) in h.variants.iter().enumerate() {
                                                        ui.selectable_value(&mut self.variant, k, v.label());
                                                    }
                                                });
                                            }
                                            if h.drm {
                                                ui.label(RichText::new(tr!("受 DRM 保护，无法下载")).size(12.5).color(p.danger));
                                            } else if h.live {
                                                ui.label(RichText::new(tr!("直播流暂不支持")).size(12.5).color(p.warning));
                                            }
                                        }
                                        Some(Ok(i)) => {
                                            let sz = i.size.map(fmt_size).unwrap_or_else(|| tr!("大小未知").into());
                                            ui.label(RichText::new(sz).size(12.5).color(p.text));
                                            let (t, c) =
                                                if i.resumable { (tr!("支持断点续传"), p.success) } else { (tr!("不支持断点续传"), p.warning) };
                                            ui.label(RichText::new(t).size(12.5).color(c));
                                        }
                                        Some(Err(e)) => {
                                            if crate::engine::sys::is_cert_error(e)
                                                && let Some(h) = crate::engine::host_key(&self.probed_url)
                                                && button(ui, Btn::Secondary, Some(Icon::Alert), tr!("信任此站点"))
                                                    .on_hover_text(trf!("今后忽略 {} 的证书错误。仅在确认该站点可信（如内网、自签名证书）时使用", h))
                                                    .clicked()
                                            {
                                                m.trust_host(&self.probed_url);
                                                self.probed_key.clear(); // 重新探测
                                            }
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(trf!("获取失败：{}", e)).size(12.5).color(p.danger),
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
                                ui.label(RichText::new(trf!("共 {} 个链接，将按服务器提供的文件名保存", n)).color(p.text));
                            });
                        }
                        if !dups.is_empty() {
                            ui.add_space(6.0);
                            let txt = if n == 1 {
                                trf!("该链接已在下载列表中（{}），仍可再次下载", dups[0].label())
                            } else {
                                trf!("其中 {} 个链接已在下载列表中，仍可再次下载", dups.len())
                            };
                            status_line(ui, Icon::Alert, p.warning, &txt);
                        }
                    });
                ui.add_space(10.0);
            }
            if n == 1 && crate::engine::remote::is_remote(&list[0]) {
                row(ui, p, tr!("用户名"), |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.user).desired_width(ui.available_width()).hint_text("anonymous"));
                });
                row(ui, p, tr!("密码"), |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.pass).password(true).desired_width(ui.available_width()));
                });
                if list[0].to_ascii_lowercase().starts_with("sftp://") {
                    row(ui, p, tr!("私钥文件"), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.key)
                                .desired_width(ui.available_width() - 40.0)
                                .hint_text(tr!("留空使用 ~/.ssh 中的默认密钥")),
                        );
                        if icon_button(ui, Icon::Folder, tr!("选择私钥文件"), true).clicked()
                            && let Some(f) = rfd::FileDialog::new().pick_file()
                        {
                            self.key = f.display().to_string();
                        }
                    });
                }
                hint(ui, p, tr!("密码使用 Windows 账户加密保存，不会以明文写入磁盘"));
            }
            let bt_files = match &self.info {
                Some(Ok(i)) if n == 1 => i.bt.as_ref().map(|b| b.files.clone()),
                _ => None,
            };
            if let Some(files) = bt_files.filter(|f| f.len() > 1) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr!("选择要下载的文件")).color(p.text));
                    if ui.small_button(tr!("全选")).clicked() {
                        self.bt_sel.iter_mut().for_each(|s| *s = true);
                    }
                    if ui.small_button(tr!("全不选")).clicked() {
                        self.bt_sel.iter_mut().for_each(|s| *s = false);
                    }
                });
                Frame::new().fill(p.bg).stroke(Stroke::new(1.0, p.stroke)).corner_radius(8).inner_margin(Margin::same(8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                        for (k, (name, len)) in files.iter().enumerate() {
                            if let Some(on) = self.bt_sel.get_mut(k) {
                                ui.horizontal(|ui| {
                                    ui.checkbox(on, name.as_str());
                                    ui.label(RichText::new(fmt_size(*len)).size(12.0).color(p.weak));
                                });
                            }
                        }
                    });
                });
                ui.add_space(8.0);
            }
            row(ui, p, tr!("保存到"), |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.dir).desired_width(ui.available_width() - 40.0));
                if icon_button(ui, Icon::Folder, tr!("选择文件夹"), true).clicked()
                    && let Some(d) = rfd::FileDialog::new().set_directory(&self.dir).pick_folder()
                {
                    self.dir = d.display().to_string();
                }
            });
            row(ui, p, tr!("连接数"), |ui| {
                ui.add(egui::Slider::new(&mut self.conns, 1..=32));
            });
            row(ui, p, "SHA-256", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.sha256)
                        .desired_width(ui.available_width())
                        .hint_text(tr!("可选，填写后完成时自动校验")),
                );
            });
            ui.add_space(6.0);
            let mut go = None;
            footer(ui, |ui| {
                let ok = n > 0;
                if ui
                    .add_enabled_ui(ok, |ui| button(ui, Btn::Primary, Some(Icon::Download), tr!("开始下载")))
                    .inner
                    .on_hover_text("Ctrl+Enter")
                    .clicked()
                {
                    go = Some(StartMode::Now);
                }
                if ui
                    .add_enabled_ui(ok, |ui| button(ui, Btn::Secondary, Some(Icon::Clock), tr!("计划下载")))
                    .inner
                    .on_hover_text(tr!("按设置中的计划时间自动开始"))
                    .clicked()
                {
                    go = Some(StartMode::Scheduled);
                }
                if ui.add_enabled_ui(ok, |ui| button(ui, Btn::Secondary, None, tr!("稍后下载"))).inner.clicked() {
                    go = Some(StartMode::Later);
                }
                if button(ui, Btn::Secondary, None, tr!("取消")).clicked() {
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
                // HLS：用户选择了非默认清晰度时，直接下载该清晰度的媒体列表
                let chosen = match &self.info {
                    Some(Ok(i)) if single && self.variant > 0 => i.hls.as_ref().and_then(|h| h.variants.get(self.variant)).map(|v| v.url.clone()),
                    _ => None,
                };
                // BT：只下载勾选的文件（全选时不限制）
                let bt_pick = (single && !self.bt_sel.is_empty() && !self.bt_sel.iter().all(|s| *s))
                    .then(|| self.bt_sel.iter().enumerate().filter(|(_, s)| **s).map(|(k, _)| k).collect::<Vec<_>>());
                for u in list {
                    let mut req = self.extra.clone();
                    req.bt_files = bt_pick.clone();
                    if single && crate::engine::remote::is_remote(&u) {
                        req.key_path = Some(self.key.clone()).filter(|k| !k.trim().is_empty());
                    }
                    let u = if single { self.with_creds(&u) } else { u };
                    req.url = chosen.clone().unwrap_or(u);
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
            if header(ui, p, Icon::Settings, p.accent, tr!("设置")) {
                result = DialogResult::Close;
            }
            let s = &mut self.s;
            egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                section(ui, p, tr!("常规"), |ui| {
                    row(ui, p, tr!("默认保存目录"), |ui| {
                        let mut d = s.dir.display().to_string();
                        if ui.add(egui::TextEdit::singleline(&mut d).desired_width(ui.available_width() - 40.0)).changed() {
                            s.dir = d.into();
                        }
                        if icon_button(ui, Icon::Folder, tr!("选择文件夹"), true).clicked()
                            && let Some(d) = rfd::FileDialog::new().set_directory(&s.dir).pick_folder()
                        {
                            s.dir = d;
                        }
                    });
                    row(ui, p, tr!("按类型分类"), |ui| {
                        ui.checkbox(&mut s.categorize, tr!("存入默认目录下的 压缩文件、视频、音乐、文档、程序 等子目录"));
                    });
                    row(ui, p, tr!("语言"), |ui| {
                        use crate::i18n::LangSetting;
                        segmented(ui, p, &mut s.lang, &[(LangSetting::System, tr!("跟随系统")), (LangSetting::Zh, "中文"), (LangSetting::En, "English")]);
                    });
                    hint(ui, p, tr!("托盘菜单在重启后切换语言"));
                    row(ui, p, tr!("外观"), |ui| {
                        segmented(ui, p, &mut s.theme, &[(ThemeMode::System, tr!("跟随系统")), (ThemeMode::Light, tr!("浅色")), (ThemeMode::Dark, tr!("深色"))]);
                    });
                });
                section(ui, p, tr!("下载"), |ui| {
                    row(ui, p, tr!("每任务连接数"), |ui| {
                        ui.add(egui::Slider::new(&mut s.conns, 1..=32));
                    });
                    row(ui, p, tr!("同时下载任务"), |ui| {
                        ui.add(egui::Slider::new(&mut s.max_active, 1..=10));
                    });
                    row(ui, p, tr!("全局限速"), |ui| {
                        ui.add(egui::DragValue::new(&mut s.speed_limit_kb).speed(64).range(0..=10_000_000).suffix(" KB/s"));
                        hint(ui, p, tr!("0 表示不限速"));
                    });
                    row(ui, p, tr!("m3u8 转 MP4"), |ui| {
                        ui.checkbox(&mut s.hls_mp4, tr!("流媒体下载完成后用 ffmpeg 无损转为 MP4"));
                    });
                    row(ui, p, tr!("ffmpeg 路径"), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut s.ffmpeg)
                                .desired_width(ui.available_width() - 40.0)
                                .hint_text(tr!("留空自动查找")),
                        );
                        if icon_button(ui, Icon::Folder, tr!("选择 ffmpeg.exe"), true).clicked()
                            && let Some(f) = rfd::FileDialog::new().add_filter("ffmpeg", &["exe"]).pick_file()
                        {
                            s.ffmpeg = f.display().to_string();
                        }
                    });
                    match crate::engine::hls::find_ffmpeg(&s.ffmpeg) {
                        Some(f) => status_line(ui, Icon::Check, p.success, &trf!("已找到 ffmpeg：{}", f.display())),
                        None => status_line(ui, Icon::Alert, p.weak, tr!("未找到 ffmpeg，m3u8 视频将保存为 .ts（可正常播放）")),
                    }
                    row(ui, p, tr!("出错自动重试"), |ui| {
                        ui.add(egui::DragValue::new(&mut s.auto_retry).range(0..=20).suffix(tr!(" 次")));
                        hint(ui, p, tr!("每次间隔 30 秒，0 表示不重试"));
                    });
                });
                section(ui, p, tr!("计划任务"), |ui| {
                    row(ui, p, tr!("启用计划"), |ui| {
                        ui.checkbox(&mut s.schedule.enabled, tr!("每天按时自动开始“计划下载”的任务"));
                    });
                    row(ui, p, tr!("开始时间"), |ui| {
                        ui.add(egui::TextEdit::singleline(&mut s.schedule.start).desired_width(70.0).hint_text("02:00"));
                        ui.label(RichText::new(tr!("停止时间")).color(p.text));
                        ui.add(egui::TextEdit::singleline(&mut s.schedule.stop).desired_width(70.0).hint_text(tr!("可留空")));
                    });
                    let bad_start = sched::parse_hm(&s.schedule.start).is_none();
                    let bad_stop = !s.schedule.stop.trim().is_empty() && sched::parse_hm(&s.schedule.stop).is_none();
                    if s.schedule.enabled && (bad_start || bad_stop) {
                        status_line(ui, Icon::Alert, p.danger, tr!("时间格式应为 HH:MM，例如 02:30"));
                    } else {
                        hint(ui, p, tr!("停止时间留空表示不自动暂停；开始晚于停止表示跨午夜"));
                    }
                });
                section(ui, p, tr!("BT / 磁力"), |ui| {
                    row(ui, p, tr!("监听端口"), |ui| {
                        ui.add(egui::DragValue::new(&mut s.bt_port).range(0..=65535));
                        hint(ui, p, tr!("0 为随机，重启后生效"));
                    });
                    row(ui, p, tr!("网络"), |ui| {
                        ui.checkbox(&mut s.bt_dht, tr!("DHT 网络（磁力链接需要）"));
                        ui.checkbox(&mut s.bt_upnp, tr!("UPnP 端口映射"));
                    });
                    row(ui, p, tr!("做种"), |ui| {
                        ui.checkbox(&mut s.bt_seed, tr!("下载完成后继续做种"));
                    });
                    row(ui, p, tr!("上传限速"), |ui| {
                        ui.add(egui::DragValue::new(&mut s.bt_upload_kb).speed(16).range(0..=10_000_000).suffix(" KB/s"));
                        hint(ui, p, tr!("0 表示不限速"));
                    });
                    row(ui, p, tr!("文件关联"), |ui| {
                        if button(ui, Btn::Secondary, None, tr!("关联磁力链接与 .torrent")).clicked() {
                            self.cleaned = crate::engine::bt::set_association(true);
                        }
                        if button(ui, Btn::Secondary, None, tr!("取消关联")).clicked() {
                            crate::engine::bt::set_association(false);
                        }
                    });
                });
                section(ui, p, tr!("网络"), |ui| {
                    row(ui, p, tr!("代理"), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut s.proxy)
                                .desired_width(ui.available_width())
                                .hint_text("http://127.0.0.1:10808"),
                        );
                    });
                    hint(ui, p, tr!("留空使用系统代理，填 direct 直连，支持 http:// 与 socks5://"));
                    row(ui, p, tr!("信任的站点"), |ui| {
                        if s.trusted_hosts.is_empty() {
                            hint(ui, p, tr!("无"));
                        } else {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                let mut remove = None;
                                for (i, h) in s.trusted_hosts.iter().enumerate() {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(h).color(p.text));
                                        if icon_button(ui, Icon::X, tr!("取消信任"), true).clicked() {
                                            remove = Some(i);
                                        }
                                    });
                                }
                                if let Some(i) = remove {
                                    s.trusted_hosts.remove(i);
                                }
                            });
                        }
                    });
                    hint(ui, p, tr!("这些站点的 HTTPS / FTPS 证书错误会被忽略；下载遇到证书不可信时可一键添加"));
                });
                section(ui, p, tr!("托盘与启动"), |ui| {
                    row(ui, p, tr!("关闭窗口"), |ui| {
                        ui.checkbox(&mut s.close_to_tray, tr!("最小化到系统托盘，后台继续下载"));
                    });
                    row(ui, p, tr!("开机自启动"), |ui| {
                        ui.checkbox(&mut s.autostart, tr!("登录 Windows 后自动启动并最小化到托盘"));
                    });
                });
                section(ui, p, tr!("剪贴板与浏览器"), |ui| {
                    row(ui, p, tr!("浏览器扩展"), |ui| {
                        if browser::connected() {
                            status_line(ui, Icon::Check, p.success, tr!("已连接"));
                        } else {
                            status_line(ui, Icon::Alert, p.weak, tr!("本次运行尚未检测到"));
                        }
                        if button(ui, Btn::Secondary, Some(Icon::External), tr!("安装向导")).clicked() {
                            result = DialogResult::OpenExtGuide;
                        }
                    });
                    row(ui, p, tr!("剪贴板监听"), |ui| {
                        ui.checkbox(&mut s.clipboard, tr!("复制下载链接时自动弹出新建窗口"));
                    });
                    row(ui, p, tr!("监听扩展名"), |ui| {
                        ui.add(egui::TextEdit::multiline(&mut s.exts).desired_rows(2).desired_width(ui.available_width()));
                    });
                    row(ui, p, tr!("扩展端口"), |ui| {
                        ui.add(egui::DragValue::new(&mut s.port).range(1024..=65535));
                        hint(ui, p, tr!("重启后生效"));
                    });
                });
                section(ui, p, tr!("安全与隐私"), |ui| {
                    row(ui, p, tr!("删除文件"), |ui| {
                        ui.checkbox(&mut s.recycle, tr!("移到回收站（可恢复）"));
                    });
                    row(ui, p, tr!("来源标记"), |ui| {
                        ui.checkbox(&mut s.motw, tr!("为下载的文件标记“来自 Internet”，打开时由系统安全检查"));
                    });
                    row(ui, p, tr!("扩展配对令牌"), |ui| {
                        let shown = format!("{}...{}", &s.token[..4.min(s.token.len())], &s.token[s.token.len().saturating_sub(4)..]);
                        ui.label(RichText::new(shown).monospace().color(p.text));
                        if button(ui, Btn::Secondary, Some(Icon::Copy), tr!("复制")).clicked() {
                            ui.ctx().copy_text(s.token.clone());
                        }
                        if button(ui, Btn::Secondary, Some(Icon::Refresh), tr!("重新生成")).on_hover_text(tr!("旧令牌立即失效；通过安装向导加载的扩展会自动更新")).clicked() {
                            s.token = crate::engine::sys::random_token();
                        }
                    });
                    hint(ui, p, tr!("浏览器扩展需携带令牌才能投递任务，防止网页或同机其他用户冒用"));
                });
                section(ui, p, tr!("更新"), |ui| {
                    row(ui, p, tr!("当前版本"), |ui| {
                        ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).color(p.text));
                        if button(ui, Btn::Secondary, Some(Icon::Refresh), tr!("检查更新")).clicked() {
                            result = DialogResult::CheckUpdate;
                        }
                    });
                    row(ui, p, tr!("自动检查"), |ui| {
                        ui.checkbox(&mut s.auto_update, tr!("每天检查一次新版本（下载后校验 SHA-256 再替换）"));
                    });
                });
                section(ui, p, tr!("存储与缓存"), |ui| {
                    row(ui, p, tr!("自动清理记录"), |ui| {
                        ui.add(egui::DragValue::new(&mut s.clean_done_days).range(0..=3650).suffix(tr!(" 天")));
                        hint(ui, p, tr!("已完成记录保留天数，0 为永久保留"));
                    });
                    row(ui, p, tr!("临时文件"), |ui| {
                        if button(ui, Btn::Secondary, Some(Icon::Trash), tr!("立即清理缓存")).clicked() {
                            let mm = m.clone();
                            m.rt.spawn_blocking(move || mm.cleanup());
                            self.cleaned = true;
                        }
                        if self.cleaned {
                            ui.label(RichText::new(tr!("已清理")).size(12.5).color(p.success));
                        }
                    });
                });
            });
            footer(ui, |ui| {
                if button(ui, Btn::Primary, Some(Icon::Check), tr!("保存")).clicked() {
                    let old_auto = m.settings.read().unwrap().autostart;
                    if self.s.autostart != old_auto && !crate::tray::set_autostart(self.s.autostart) {
                        self.s.autostart = old_auto;
                    }
                    if self.s.token != m.settings.read().unwrap().token {
                        crate::browser::refresh_config(&self.s.token, self.s.port);
                    }
                    m.apply_settings(self.s.clone());
                    result = DialogResult::Close;
                }
                if button(ui, Btn::Secondary, None, tr!("取消")).clicked() {
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
            if header(ui, p, Icon::Trash, p.danger, tr!("删除任务")) {
                result = DialogResult::Close;
            }
            ui.label(RichText::new(trf!("确定删除选中的 {} 个任务吗？", self.ids.len())).color(p.text));
            ui.add_space(4.0);
            ui.checkbox(&mut self.delete_file, tr!("同时删除已下载的文件"));
            ui.add_space(6.0);
            footer(ui, |ui| {
                if button(ui, Btn::Danger, Some(Icon::Trash), tr!("删除")).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    m.remove(&self.ids, self.delete_file);
                    result = DialogResult::Close;
                }
                if button(ui, Btn::Secondary, None, tr!("取消")).clicked() {
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
            if header(ui, p, Icon::Refresh, p.accent, tr!("更新下载链接")) {
                result = DialogResult::Close;
            }
            ui.add(egui::Label::new(RichText::new(&self.name).font(bold(14.0)).color(p.text)).truncate());
            hint(ui, p, tr!("链接过期或失效时，粘贴同一文件的新链接即可从断点继续下载"));
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
                    ui.label(RichText::new(tr!("正在验证新链接")).size(13.0).color(p.weak));
                });
            } else if let Some(e) = &self.err {
                status_line(ui, Icon::Alert, p.danger, e);
            }
            footer(ui, |ui| {
                let ok = !self.pending && self.url.trim().starts_with("http");
                if ui.add_enabled_ui(ok, |ui| button(ui, Btn::Primary, Some(Icon::Refresh), tr!("更新并继续"))).inner.clicked() {
                    self.pending = true;
                    self.err = None;
                    let (mm, id, url, slot) = (m.clone(), self.id, self.url.trim().to_string(), self.slot.clone());
                    m.rt.spawn(async move {
                        let r = mm.relink(id, url).await;
                        *slot.lock().unwrap() = Some(r);
                        mm.repaint();
                    });
                }
                if button(ui, Btn::Secondary, None, tr!("取消")).clicked() {
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
            if header(ui, p, Icon::External, p.accent, tr!("安装浏览器扩展")) {
                result = DialogResult::Close;
            }
            ui.label(RichText::new(tr!("安装后，浏览器中的下载会自动交给 SwiftDL 多线程加速，并保留登录状态。")).color(p.text));
            hint(ui, p, tr!("Chrome 与 Edge 只允许用户本人加载商店之外的扩展，下面两步即可完成。"));
            ui.add_space(10.0);
            if connected {
                status_line(ui, Icon::Check, p.success, tr!("扩展已连接，安装完成"));
            } else if self.browsers.is_empty() {
                status_line(ui, Icon::Alert, p.warning, tr!("未检测到 Chrome 或 Edge，可手动加载扩展文件夹"));
                ui.add_space(6.0);
                if button(ui, Btn::Primary, Some(Icon::Folder), tr!("打开扩展文件夹")).clicked() {
                    let res = {
                        let st = m.settings.read().unwrap();
                        browser::extract(&st.token, st.port)
                    };
                    match res {
                        Ok(dir) => {
                            ctx.copy_text(dir.display().to_string());
                            let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                        }
                        Err(e) => self.err = Some(trf!("释放扩展失败：{}", e)),
                    }
                }
            } else {
                ui.horizontal(|ui| {
                    for (i, (b, exe)) in self.browsers.clone().into_iter().enumerate() {
                        let kind = if i == 0 { Btn::Primary } else { Btn::Secondary };
                        let label = trf!("在 {} 中安装", if b == Browser::Edge { "Edge" } else { "Chrome" });
                        if button(ui, kind, Some(Icon::External), &label).on_hover_text(b.name()).clicked() {
                            let res = {
                                let st = m.settings.read().unwrap();
                                browser::launch(&exe, b, &st.token, st.port)
                            };
                            match res {
                                Ok(dir) => {
                                    ctx.copy_text(dir.display().to_string());
                                    self.launched = Some(b);
                                    self.err = None;
                                }
                                Err(e) => self.err = Some(trf!("启动浏览器失败：{}", e)),
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
                        step(ui, p, 1, tr!("在打开的扩展页面中，开启右上角（Edge 为左下角）的“开发者模式”"));
                        step(ui, p, 2, tr!("把资源管理器中已选中的 extension 文件夹拖到扩展页面上；或点击“加载解压缩的扩展”，按 Ctrl+V 粘贴已复制的路径"));
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(14.0));
                            ui.label(RichText::new(tr!("等待扩展连接，安装完成后会自动提示")).size(13.0).color(p.weak));
                        });
                    });
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            }
            footer(ui, |ui| {
                if connected {
                    if button(ui, Btn::Primary, Some(Icon::Check), tr!("完成")).clicked() {
                        done = true;
                    }
                } else {
                    if button(ui, Btn::Secondary, None, tr!("稍后")).clicked() {
                        result = DialogResult::Close;
                    }
                    if button(ui, Btn::Secondary, None, tr!("已安装，不再提示")).clicked() {
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
            if header(ui, p, Icon::Download, p.accent, tr!("关闭 SwiftDL")) {
                choice = CloseChoice::Cancel;
            }
            ui.label(RichText::new(tr!("关闭窗口后，是否在系统托盘中继续下载？")).color(p.text));
            ui.add_space(4.0);
            ui.checkbox(&mut self.remember, tr!("记住我的选择（可在设置中修改）"));
            footer(ui, |ui| {
                if button(ui, Btn::Primary, None, tr!("最小化到托盘")).clicked() {
                    choice = CloseChoice::Tray { remember: self.remember };
                }
                if button(ui, Btn::Secondary, None, tr!("退出程序")).clicked() {
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
        if header(ui, p, Icon::Clock, p.warning, tr!("全部下载已完成")) {
            r = Some(false);
        }
        ui.label(RichText::new(trf!("{} 秒后将{}", remaining, action)).font(bold(16.0)).color(p.text));
        let frac = remaining as f32 / 60.0;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
        ui.painter().rect_filled(rect, 3, p.track);
        let mut f = rect;
        f.max.x = rect.min.x + rect.width() * frac;
        ui.painter().rect_filled(f, 3, p.warning);
        footer(ui, |ui| {
            if button(ui, Btn::Primary, None, tr!("立即执行")).clicked() {
                r = Some(true);
            }
            if button(ui, Btn::Secondary, None, tr!("取消")).clicked() {
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
    if s == Status::Paused && scheduled { tr!("已计划") } else { s.label() }
}
