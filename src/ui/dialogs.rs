use super::widgets::fmt_size;
use crate::engine::probe::{self, Info};
use crate::engine::{AddReq, Manager, Settings};
use eframe::egui::{self, Id, Key, Modal, RichText};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type ProbeSlot = Arc<Mutex<Option<(String, Result<Info, String>)>>>;

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

pub enum DialogResult {
    None,
    Close,
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
        let resp = Modal::new(Id::unique("add_dialog")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading("新建下载");
            ui.add_space(6.0);
            ui.label("下载链接（每行一个）");
            let te = ui.add(
                egui::TextEdit::multiline(&mut self.urls)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text("https://…"),
            );
            if self.focus {
                te.request_focus();
                self.focus = false;
            }
            let n = self.url_list().len();
            egui::Grid::new("add_grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                if n <= 1 {
                    ui.label("文件名");
                    ui.horizontal(|ui| {
                        if ui
                            .add(egui::TextEdit::singleline(&mut self.name).desired_width(380.0).hint_text("自动识别"))
                            .changed()
                        {
                            self.name_edited = true;
                        }
                        match &self.info {
                            None if n == 1 => {
                                ui.spinner();
                            }
                            Some(Ok(i)) => {
                                let sz = i.size.map(fmt_size).unwrap_or_else(|| "未知大小".into());
                                let rs = if i.resumable { "" } else { " · 不支持续传" };
                                ui.label(RichText::new(format!("{sz}{rs}")).weak());
                            }
                            Some(Err(e)) => {
                                ui.label(RichText::new("探测失败").color(super::widgets::RED)).on_hover_text(e);
                            }
                            _ => {}
                        }
                    });
                    ui.end_row();
                }
                ui.label("保存到");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.dir).desired_width(380.0));
                    if ui.button("浏览…").clicked()
                        && let Some(p) = rfd::FileDialog::new().set_directory(&self.dir).pick_folder()
                    {
                        self.dir = p.display().to_string();
                    }
                });
                ui.end_row();
                ui.label("连接数");
                ui.add(egui::Slider::new(&mut self.conns, 1..=32));
                ui.end_row();
            });
            ui.add_space(8.0);
            let mut go = None;
            ui.horizontal(|ui| {
                let ok = n > 0;
                if ui.add_enabled(ok, egui::Button::new(RichText::new("  开始下载  ").strong())).clicked() {
                    go = Some(true);
                }
                if ui.add_enabled(ok, egui::Button::new("稍后下载")).clicked() {
                    go = Some(false);
                }
                if ui.button("取消").clicked() {
                    result = DialogResult::Close;
                }
                if n > 1 {
                    ui.label(RichText::new(format!("共 {n} 个链接")).weak());
                }
            });
            // Ctrl+Enter 开始（多行输入框内回车用于换行）
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
        if resp.should_close() {
            result = DialogResult::Close;
        }
        result
    }
}

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
        let resp = Modal::new(Id::unique("settings")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading("设置");
            ui.add_space(6.0);
            let s = &mut self.s;
            egui::Grid::new("set_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("默认保存目录");
                ui.horizontal(|ui| {
                    let mut d = s.dir.display().to_string();
                    if ui.add(egui::TextEdit::singleline(&mut d).desired_width(300.0)).changed() {
                        s.dir = d.into();
                    }
                    if ui.button("浏览…").clicked()
                        && let Some(p) = rfd::FileDialog::new().set_directory(&s.dir).pick_folder()
                    {
                        s.dir = p;
                    }
                });
                ui.end_row();
                ui.label("每任务连接数");
                ui.add(egui::Slider::new(&mut s.conns, 1..=32));
                ui.end_row();
                ui.label("同时下载任务");
                ui.add(egui::Slider::new(&mut s.max_active, 1..=10));
                ui.end_row();
                ui.label("全局限速");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut s.speed_limit_kb).speed(64).suffix(" KB/s"));
                    ui.label(RichText::new("0 = 不限速").weak());
                });
                ui.end_row();
                ui.label("代理");
                ui.add(
                    egui::TextEdit::singleline(&mut s.proxy)
                        .desired_width(300.0)
                        .hint_text("留空=系统代理  direct=直连  http://127.0.0.1:10808"),
                );
                ui.end_row();
                ui.label("剪贴板监听");
                ui.checkbox(&mut s.clipboard, "复制下载链接时自动弹出新建窗口");
                ui.end_row();
                ui.label("监听扩展名");
                ui.add(egui::TextEdit::multiline(&mut s.exts).desired_rows(2).desired_width(300.0));
                ui.end_row();
                ui.label("自动清理");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut s.clean_done_days).range(0..=3650).suffix(" 天"));
                    ui.label(RichText::new("后移除已完成记录（0 = 不清理）").weak());
                });
                ui.end_row();
                ui.label("缓存");
                ui.horizontal(|ui| {
                    if ui.button("立即清理缓存").clicked() {
                        let mm = m.clone();
                        m.rt.spawn_blocking(move || mm.cleanup());
                        self.cleaned = true;
                    }
                    if self.cleaned {
                        ui.label(RichText::new("已清理孤立临时文件").weak());
                    }
                });
                ui.end_row();
                ui.label("扩展端口");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut s.port).range(1024..=65535));
                    ui.label(RichText::new("重启后生效").weak());
                });
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("  保存  ").clicked() {
                    m.apply_settings(self.s.clone());
                    result = DialogResult::Close;
                }
                if ui.button("取消").clicked() {
                    result = DialogResult::Close;
                }
            });
        });
        if resp.should_close() {
            result = DialogResult::Close;
        }
        result
    }
}

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
        let resp = Modal::new(Id::unique("delete")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.label(format!("删除选中的 {} 个任务？", self.ids.len()));
            ui.checkbox(&mut self.delete_file, "同时删除已下载的文件");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let del = ui.button(RichText::new("  删除  ").color(super::widgets::RED));
                if del.clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    m.remove(&self.ids, self.delete_file);
                    result = DialogResult::Close;
                }
                if ui.button("取消").clicked() {
                    result = DialogResult::Close;
                }
            });
        });
        if resp.should_close() {
            result = DialogResult::Close;
        }
        result
    }
}
