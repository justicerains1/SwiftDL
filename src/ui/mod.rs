mod dialogs;
mod widgets;

use crate::engine::{AddReq, Manager, Status, Task, UiEvent};
use dialogs::{AddDialog, DeleteDialog, DialogResult, SettingsDialog};
use eframe::egui::{self, Align2, Color32, FontId, Key, Rect, RichText, Sense, Vec2, pos2};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering::*;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use widgets::*;

const ROW_H: f32 = 26.0;
// 列宽：文件名自适应，其余固定
const COLS: [(&str, f32); 5] = [("大小", 90.0), ("进度", 170.0), ("速度", 96.0), ("剩余", 80.0), ("状态", 66.0)];

pub struct App {
    m: Arc<Manager>,
    rx: Receiver<UiEvent>,
    sel: HashSet<u64>,
    anchor: Option<usize>,
    add: Option<AddDialog>,
    settings: Option<SettingsDialog>,
    delete: Option<DeleteDialog>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, m: Arc<Manager>, rx: Receiver<UiEvent>) -> Self {
        setup_fonts(&cc.egui_ctx);
        cc.egui_ctx.all_styles_mut(|s| {
            s.spacing.button_padding = Vec2::new(10.0, 5.0);
            s.spacing.item_spacing = Vec2::new(8.0, 6.0);
        });
        let _ = m.ui_ctx.set(cc.egui_ctx.clone());
        Self { m, rx, sel: HashSet::new(), anchor: None, add: None, settings: None, delete: None }
    }

    fn open_add(&mut self, ctx: &egui::Context, reqs: Vec<AddReq>) {
        match &mut self.add {
            Some(d) => d.append(reqs),
            None => self.add = Some(AddDialog::new(&self.m, reqs)),
        }
        bring_to_front(ctx);
    }

    fn selected(&self) -> Vec<u64> {
        self.sel.iter().copied().collect()
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("＋ 新建").on_hover_text("Ctrl+N").clicked() {
                self.open_add(ui.ctx(), vec![]);
            }
            ui.separator();
            let has = !self.sel.is_empty();
            if ui.add_enabled(has, egui::Button::new("▶ 开始")).clicked() {
                self.selected().into_iter().for_each(|id| self.m.start(id));
            }
            if ui.add_enabled(has, egui::Button::new("⏸ 暂停")).clicked() {
                self.selected().into_iter().for_each(|id| self.m.pause(id));
            }
            if ui.add_enabled(has, egui::Button::new("🗑 删除")).on_hover_text("Delete").clicked() {
                self.delete = Some(DeleteDialog::new(self.selected()));
            }
            ui.separator();
            if ui.button("全部开始").clicked() {
                let ids: Vec<u64> = self.m.tasks.read().unwrap().iter().map(|t| t.id).collect();
                ids.into_iter().for_each(|id| self.m.start(id));
            }
            if ui.button("全部暂停").clicked() {
                let ids: Vec<u64> = self.m.tasks.read().unwrap().iter().map(|t| t.id).collect();
                ids.into_iter().for_each(|id| self.m.pause(id));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⚙ 设置").clicked() {
                    self.settings = Some(SettingsDialog::new(&self.m));
                }
            });
        });
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let tasks = self.m.tasks.read().unwrap();
        let speed: u64 = tasks.iter().map(|t| t.speed.load(Relaxed)).sum();
        let count = |s| tasks.iter().filter(|t| t.status() == s).count();
        let lim = self.m.settings.read().unwrap().speed_limit_kb;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("↓ {}", if speed > 0 { fmt_speed(speed) } else { "0 B/s".into() })).strong());
            ui.separator();
            ui.label(format!("下载中 {}  ·  排队 {}  ·  完成 {}", count(Status::Running), count(Status::Queued), count(Status::Done)));
            if lim > 0 {
                ui.separator();
                ui.label(format!("限速 {}/s", fmt_size(lim * 1024)));
            }
        });
    }

    fn details(&self, ui: &mut egui::Ui, t: &Task) {
        let (name, dir, size, err, resumable) = {
            let m = t.meta.lock().unwrap();
            (m.name.clone(), m.dir.clone(), m.size, m.error.clone(), m.resumable)
        };
        segment_bar(ui, t);
        ui.horizontal(|ui| {
            ui.label(RichText::new(if name.is_empty() { "…" } else { &name }).strong());
            ui.label(RichText::new(format!(
                "{} / {}  ·  连接 {}/{}{}",
                fmt_size(t.downloaded.load(Relaxed)),
                size.map(fmt_size).unwrap_or_else(|| "未知".into()),
                t.active_conns.load(Relaxed),
                t.conns,
                if t.meta.lock().unwrap().probed && !resumable { "  ·  不支持续传" } else { "" }
            ))
            .weak());
        });
        ui.label(RichText::new(format!("{}  →  {}", t.req.url, dir.display())).weak().small());
        if let Some(e) = err {
            ui.label(RichText::new(format!("错误: {e}")).color(RED));
        }
    }

    fn header(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), Sense::hover());
        let p = ui.painter();
        let font = FontId::proportional(12.5);
        let c = ui.visuals().weak_text_color();
        p.text(pos2(rect.min.x + 8.0, rect.center().y), Align2::LEFT_CENTER, "文件名", font.clone(), c);
        let mut x = rect.max.x;
        for (name, w) in COLS.iter().rev() {
            x -= w;
            p.text(pos2(x + 6.0, rect.center().y), Align2::LEFT_CENTER, *name, font.clone(), c);
        }
        p.hline(rect.x_range(), rect.max.y, ui.visuals().widgets.noninteractive.bg_stroke);
    }

    fn list(&mut self, ui: &mut egui::Ui) {
        let tasks: Vec<Arc<Task>> = self.m.tasks.read().unwrap().clone();
        self.header(ui);
        if tasks.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Ctrl+N 新建任务，或复制下载链接 / 使用浏览器扩展").weak());
            });
            return;
        }
        let mods = ui.input(|i| i.modifiers);
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, ROW_H, tasks.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let t = &tasks[i];
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
                let selected = self.sel.contains(&t.id);
                self.paint_row(ui, rect, t, selected, resp.hovered());
                if resp.clicked() {
                    if mods.shift && let Some(a) = self.anchor {
                        let (lo, hi) = (a.min(i), a.max(i));
                        if !mods.command {
                            self.sel.clear();
                        }
                        self.sel.extend(tasks[lo..=hi].iter().map(|t| t.id));
                    } else if mods.command {
                        if !self.sel.remove(&t.id) {
                            self.sel.insert(t.id);
                        }
                        self.anchor = Some(i);
                    } else {
                        self.sel.clear();
                        self.sel.insert(t.id);
                        self.anchor = Some(i);
                    }
                }
                if resp.double_clicked() {
                    match t.status() {
                        Status::Done => open_path(&t.final_path(), false),
                        Status::Running | Status::Queued => self.m.pause(t.id),
                        _ => self.m.start(t.id),
                    }
                }
                if resp.secondary_clicked() && !selected {
                    self.sel.clear();
                    self.sel.insert(t.id);
                    self.anchor = Some(i);
                }
                resp.context_menu(|ui| self.row_menu(ui, t));
            }
        });
    }

    fn row_menu(&mut self, ui: &mut egui::Ui, t: &Arc<Task>) {
        let st = t.status();
        if st == Status::Done && ui.button("打开文件").clicked() {
            open_path(&t.final_path(), false);
            ui.close();
        }
        if ui.button("打开所在文件夹").clicked() {
            let p = if st == Status::Done { t.final_path() } else { t.part_path() };
            open_path(&p, true);
            ui.close();
        }
        ui.separator();
        if matches!(st, Status::Paused | Status::Error) && ui.button("开始").clicked() {
            self.selected().into_iter().for_each(|id| self.m.start(id));
            ui.close();
        }
        if matches!(st, Status::Running | Status::Queued) && ui.button("暂停").clicked() {
            self.selected().into_iter().for_each(|id| self.m.pause(id));
            ui.close();
        }
        if st != Status::Running && ui.button("重新下载").clicked() {
            self.m.restart(t.id);
            ui.close();
        }
        if ui.button("复制链接").clicked() {
            *self.m.clip_ignore.lock().unwrap() = t.req.url.clone();
            ui.ctx().copy_text(t.req.url.clone());
            ui.close();
        }
        ui.separator();
        if ui.button("删除").clicked() {
            self.delete = Some(DeleteDialog::new(self.selected()));
            ui.close();
        }
    }

    fn paint_row(&self, ui: &egui::Ui, rect: Rect, t: &Task, selected: bool, hovered: bool) {
        let v = ui.visuals();
        let p = ui.painter();
        if selected {
            p.rect_filled(rect, 0.0, v.selection.bg_fill.gamma_multiply(0.55));
        } else if hovered {
            p.rect_filled(rect, 0.0, v.widgets.hovered.weak_bg_fill.gamma_multiply(0.5));
        }
        let st = t.status();
        let (name, size) = {
            let m = t.meta.lock().unwrap();
            let n = if m.name.is_empty() { crate::engine::probe::url_name(&t.req.url).unwrap_or_else(|| t.req.url.clone()) } else { m.name.clone() };
            (n, m.size)
        };
        let done = t.downloaded.load(Relaxed);
        let speed = t.speed.load(Relaxed);
        let font = FontId::proportional(13.5);
        let text = v.text_color();
        let weak = v.weak_text_color();
        let cy = rect.center().y;
        let cols_w: f32 = COLS.iter().map(|c| c.1).sum();

        // 文件名（裁剪）
        let name_rect = Rect::from_min_max(rect.min, pos2(rect.max.x - cols_w - 8.0, rect.max.y));
        let galley = p.layout_no_wrap(name, font.clone(), text);
        p.with_clip_rect(name_rect).galley(pos2(rect.min.x + 8.0, cy - galley.size().y / 2.0), galley, text);

        let mut x = rect.max.x - cols_w;
        let cell = |x: &mut f32, w: f32, s: String, c: Color32| {
            p.text(pos2(*x + 6.0, cy), Align2::LEFT_CENTER, s, font.clone(), c);
            *x += w;
        };
        cell(&mut x, COLS[0].1, size.map(fmt_size).unwrap_or_default(), weak);
        // 进度条列
        let frac = match (st, size) {
            (Status::Done, _) => 1.0,
            (_, Some(s)) if s > 0 => done as f32 / s as f32,
            _ => 0.0,
        };
        let bar = Rect::from_center_size(pos2(x + COLS[1].1 / 2.0 - 30.0, cy), Vec2::new(COLS[1].1 - 72.0, 6.0));
        let color = match st {
            Status::Done => GREEN,
            Status::Error => RED,
            Status::Running => ACCENT,
            _ => GRAY,
        };
        progress(ui, bar, frac, color);
        let pct = if size.is_some() || st == Status::Done { format!("{:.1}%", frac * 100.0) } else { fmt_size(done) };
        p.text(pos2(bar.max.x + 6.0, cy), Align2::LEFT_CENTER, pct, FontId::proportional(12.0), weak);
        x += COLS[1].1;
        let running = st == Status::Running;
        cell(&mut x, COLS[2].1, if running { fmt_speed(speed) } else { String::new() }, text);
        let eta = match size {
            Some(s) if running && speed > 0 => fmt_eta(s.saturating_sub(done) / speed),
            _ => String::new(),
        };
        cell(&mut x, COLS[3].1, eta, weak);
        let sc = match st {
            Status::Error => RED,
            Status::Done => GREEN,
            Status::Running => ACCENT,
            _ => weak,
        };
        cell(&mut x, COLS[4].1, st.label().into(), sc);
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.add.is_some() || self.settings.is_some() || self.delete.is_some() {
            return;
        }
        let (new, del, all, space, paste) = ctx.input(|i| {
            let paste = i.events.iter().find_map(|e| match e {
                egui::Event::Paste(s) => Some(s.clone()),
                _ => None,
            });
            (
                i.modifiers.command && i.key_pressed(Key::N),
                i.key_pressed(Key::Delete),
                i.modifiers.command && i.key_pressed(Key::A),
                i.key_pressed(Key::Space),
                paste,
            )
        });
        if new {
            self.open_add(ctx, vec![]);
        }
        if del && !self.sel.is_empty() {
            self.delete = Some(DeleteDialog::new(self.selected()));
        }
        if all {
            self.sel = self.m.tasks.read().unwrap().iter().map(|t| t.id).collect();
        }
        if space {
            for id in self.selected() {
                match self.m.get(id).map(|t| t.status()) {
                    Some(Status::Running | Status::Queued) => self.m.pause(id),
                    _ => self.m.start(id),
                }
            }
        }
        // 主界面直接 Ctrl+V 粘贴链接
        if let Some(text) = paste {
            let reqs: Vec<AddReq> = text
                .lines()
                .map(str::trim)
                .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
                .map(|u| AddReq { url: u.into(), ..Default::default() })
                .collect();
            if !reqs.is_empty() {
                self.open_add(ctx, reqs);
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                UiEvent::Add(r) => self.open_add(&ctx, r),
                UiEvent::Show => bring_to_front(&ctx),
            }
        }
        self.shortcuts(&ctx);

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        let detail = (self.sel.len() == 1).then(|| self.sel.iter().next().and_then(|&id| self.m.get(id))).flatten();
        if let Some(t) = &detail {
            egui::Panel::bottom("detail").show(ui, |ui| {
                ui.add_space(4.0);
                self.details(ui, t);
                ui.add_space(2.0);
            });
        }
        egui::CentralPanel::default_margins().show(ui, |ui| self.list(ui));

        if let Some(d) = &mut self.add
            && matches!(d.show(&ctx, &self.m), DialogResult::Close)
        {
            self.add = None;
        }
        if let Some(d) = &mut self.settings
            && matches!(d.show(&ctx, &self.m), DialogResult::Close)
        {
            self.settings = None;
        }
        if let Some(d) = &mut self.delete
            && matches!(d.show(&ctx, &self.m), DialogResult::Close)
        {
            for id in &d.ids {
                self.sel.remove(id);
            }
            self.delete = None;
        }

        // 仅在有任务活动时定时刷新，空闲时零 CPU
        if self.m.tasks.read().unwrap().iter().any(|t| t.status() == Status::Running) {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

fn bring_to_front(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

fn open_path(p: &std::path::Path, select: bool) {
    let mut c = std::process::Command::new("explorer");
    if select {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(p);
        c.arg(arg);
    } else {
        c.arg(p);
    }
    let _ = c.spawn();
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for f in ["msyh.ttc", "msyh.ttf", "Deng.ttf", "simhei.ttf", "simsun.ttc"] {
        let path = std::path::Path::new("C:\\Windows\\Fonts").join(f);
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("cjk".into(), Arc::new(egui::FontData::from_owned(bytes)));
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.entry(fam).or_default().push("cjk".into());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}
