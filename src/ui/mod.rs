mod dialogs;
mod icons;
mod theme;
mod widgets;

pub use theme::app_icon;

use crate::engine::{AddReq, Manager, Status, Task, ThemeMode, UiEvent};
use dialogs::{AddDialog, DeleteDialog, DialogResult, SettingsDialog};
use eframe::egui::{
    self, Align, Align2, Frame, Id, Key, Layout, Margin, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};
use icons::Icon;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering::*;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use theme::Pal;
use widgets::*;

const ROW_H: f32 = 64.0;
const SAMPLE: Duration = Duration::from_millis(500);
const HISTORY: usize = 120; // 60 秒

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Running,
    Queued,
    Unfinished,
    Done,
}

impl Filter {
    const ALL: [Filter; 5] = [Self::All, Self::Running, Self::Queued, Self::Unfinished, Self::Done];
    fn label(self) -> &'static str {
        match self {
            Self::All => "全部任务",
            Self::Running => "下载中",
            Self::Queued => "排队中",
            Self::Unfinished => "未完成",
            Self::Done => "已完成",
        }
    }
    fn icon(self) -> Icon {
        match self {
            Self::All => Icon::Download,
            Self::Running => Icon::Activity,
            Self::Queued => Icon::Clock,
            Self::Unfinished => Icon::PauseCircle,
            Self::Done => Icon::Check,
        }
    }
    fn matches(self, s: Status) -> bool {
        match self {
            Self::All => true,
            Self::Running => s == Status::Running,
            Self::Queued => s == Status::Queued,
            Self::Unfinished => matches!(s, Status::Paused | Status::Error),
            Self::Done => s == Status::Done,
        }
    }
}

struct Toast {
    task: u64,
    ok: bool,
    name: String,
    born: Instant,
}

pub struct App {
    m: Arc<Manager>,
    rx: Receiver<UiEvent>,
    sel: HashSet<u64>,
    anchor: Option<usize>,
    add: Option<AddDialog>,
    settings: Option<SettingsDialog>,
    delete: Option<DeleteDialog>,
    filter: Filter,
    search: String,
    focus_search: bool,
    history: VecDeque<u64>,
    last_sample: Instant,
    prev: HashMap<u64, Status>,
    toasts: Vec<Toast>,
    theme: Option<ThemeMode>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, m: Arc<Manager>, rx: Receiver<UiEvent>) -> Self {
        theme::install(&cc.egui_ctx);
        let _ = m.ui_ctx.set(cc.egui_ctx.clone());
        Self {
            m,
            rx,
            sel: HashSet::new(),
            anchor: None,
            add: None,
            settings: None,
            delete: None,
            filter: Filter::All,
            search: String::new(),
            focus_search: false,
            history: VecDeque::with_capacity(HISTORY + 1),
            last_sample: Instant::now(),
            prev: HashMap::new(),
            toasts: Vec::new(),
            theme: None,
        }
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

    fn all_ids(&self) -> Vec<u64> {
        self.m.tasks.read().unwrap().iter().map(|t| t.id).collect()
    }

    fn visible_tasks(&self) -> Vec<Arc<Task>> {
        let q = self.search.trim().to_lowercase();
        self.m
            .tasks
            .read()
            .unwrap()
            .iter()
            .filter(|t| self.filter.matches(t.status()))
            .filter(|t| q.is_empty() || display_name(t).to_lowercase().contains(&q) || t.req.url.to_lowercase().contains(&q))
            .cloned()
            .collect()
    }

    /// 每 500ms 采样总速度，供侧栏折线使用
    fn sample_speed(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_sample) > Duration::from_secs(60) {
            self.history.clear();
            self.last_sample = now;
        }
        while now.duration_since(self.last_sample) >= SAMPLE {
            self.last_sample += SAMPLE;
            let total: u64 = self.m.tasks.read().unwrap().iter().map(|t| t.speed.load(Relaxed)).sum();
            self.history.push_back(total);
            if self.history.len() > HISTORY {
                self.history.pop_front();
            }
        }
    }

    /// 任务状态变化时弹出提示
    fn detect_changes(&mut self) {
        let tasks = self.m.tasks.read().unwrap().clone();
        for t in &tasks {
            let s = t.status();
            if let Some(prev) = self.prev.insert(t.id, s)
                && prev != s
                && (s == Status::Done || (s == Status::Error && prev == Status::Running))
            {
                self.toasts.push(Toast { task: t.id, ok: s == Status::Done, name: display_name(t), born: Instant::now() });
                if self.toasts.len() > 4 {
                    self.toasts.remove(0);
                }
            }
        }
        if self.prev.len() > tasks.len() {
            self.prev.retain(|id, _| tasks.iter().any(|t| t.id == *id));
            self.sel.retain(|id| tasks.iter().any(|t| t.id == *id));
        }
    }

    // ───────────── 侧边栏 ─────────────
    fn sidebar(&mut self, ui: &mut Ui) {
        let p = theme::pal(ui);
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
            logo(ui, r);
            ui.add_space(2.0);
            ui.label(RichText::new("SwiftDL").font(bold(18.0)).color(p.text));
        });
        ui.add_space(22.0);
        ui.label(RichText::new("分类").size(12.0).color(p.weak));
        ui.add_space(2.0);
        let counts: Vec<usize> = {
            let tasks = self.m.tasks.read().unwrap();
            Filter::ALL.iter().map(|f| tasks.iter().filter(|t| f.matches(t.status())).count()).collect()
        };
        ui.spacing_mut().item_spacing.y = 2.0;
        for (f, n) in Filter::ALL.into_iter().zip(counts) {
            if nav_item(ui, &p, f.icon(), f.label(), n, self.filter == f).clicked() {
                self.filter = f;
            }
        }

        // 底部速度卡片
        let limit = self.m.settings.read().unwrap().speed_limit_kb;
        let card_h = if limit > 0 { 132.0 } else { 114.0 };
        let gap = ui.available_height() - card_h;
        if gap > 0.0 {
            ui.add_space(gap);
        }
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), card_h), Sense::hover());
        let painter = ui.painter();
        painter.rect(rect, 10, p.panel, Stroke::new(1.0, p.stroke), StrokeKind::Inside);
        let inner = rect.shrink(12.0);
        icons::paint(painter, Rect::from_min_size(inner.min, vec2(14.0, 14.0)), Icon::ArrowDown, p.accent);
        painter.text(pos2(inner.min.x + 20.0, inner.min.y + 7.0), Align2::LEFT_CENTER, "总速度", prop(12.0), p.weak);
        let total = self.history.back().copied().unwrap_or(0);
        painter.text(pos2(inner.min.x, inner.min.y + 32.0), Align2::LEFT_CENTER, fmt_speed(total), bold(19.0), p.text);
        let spark = Rect::from_min_size(pos2(inner.min.x, inner.min.y + 48.0), vec2(inner.width(), 40.0));
        sparkline(ui, spark, &self.history, p.accent);
        if limit > 0 {
            ui.painter().text(
                pos2(inner.min.x, inner.max.y - 6.0),
                Align2::LEFT_CENTER,
                format!("已限速 {}", fmt_speed(limit * 1024)),
                prop(12.0),
                p.warning,
            );
        }
    }

    // ───────────── 顶栏 ─────────────
    fn topbar(&mut self, ui: &mut Ui) {
        let p = theme::pal(ui);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if button(ui, Btn::Primary, Some(Icon::Plus), "新建下载").on_hover_text("Ctrl+N").clicked() {
                self.open_add(ui.ctx(), vec![]);
            }
            ui.add_space(10.0);
            let has = !self.sel.is_empty();
            if icon_button(ui, Icon::Play, "开始（Space）", has).clicked() {
                self.selected().into_iter().for_each(|id| self.m.start(id));
            }
            if icon_button(ui, Icon::Pause, "暂停（Space）", has).clicked() {
                self.selected().into_iter().for_each(|id| self.m.pause(id));
            }
            if icon_button(ui, Icon::Trash, "删除（Delete）", has).clicked() {
                self.delete = Some(DeleteDialog::new(self.selected()));
            }
            ui.add_space(6.0);
            separator_v(ui, &p);
            ui.add_space(6.0);
            if button(ui, Btn::Secondary, Some(Icon::Play), "全部开始").clicked() {
                self.all_ids().into_iter().for_each(|id| self.m.start(id));
            }
            ui.add_space(4.0);
            if button(ui, Btn::Secondary, Some(Icon::Pause), "全部暂停").clicked() {
                self.all_ids().into_iter().for_each(|id| self.m.pause(id));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, Icon::Settings, "设置", true).clicked() {
                    self.settings = Some(SettingsDialog::new(&self.m));
                }
                ui.add_space(6.0);
                self.search_box(ui, &p);
            });
        });
    }

    fn search_box(&mut self, ui: &mut Ui, p: &Pal) {
        let w = ui.available_width().clamp(120.0, 260.0);
        let (rect, _) = ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
        let id = Id::unique("search_box");
        let focused = ui.memory(|m| m.has_focus(id));
        let bg = ui.visuals().text_edit_bg_color();
        let stroke = if focused { Stroke::new(1.5, p.accent) } else { Stroke::new(1.0, p.stroke) };
        ui.painter().rect(rect, 8, bg, stroke, StrokeKind::Inside);
        icons::paint(ui.painter(), Rect::from_min_size(pos2(rect.min.x + 10.0, rect.center().y - 7.0), vec2(14.0, 14.0)), Icon::Search, p.weak);
        let clear_w = if self.search.is_empty() { 8.0 } else { 30.0 };
        let te_rect = Rect::from_min_max(pos2(rect.min.x + 32.0, rect.center().y - 11.0), pos2(rect.max.x - clear_w, rect.center().y + 11.0));
        let te = ui.put(
            te_rect,
            egui::TextEdit::singleline(&mut self.search).id(id).frame(Frame::NONE).hint_text("搜索文件名或链接"),
        );
        if self.focus_search {
            te.request_focus();
            self.focus_search = false;
        }
        if te.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
            self.search.clear();
        }
        if !self.search.is_empty() {
            let r = Rect::from_center_size(pos2(rect.max.x - 16.0, rect.center().y), vec2(22.0, 22.0));
            let resp = ui.interact(r, id.with("clear"), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(r, 6, p.hover);
            }
            icons::paint(ui.painter(), r.shrink(5.0), Icon::X, p.weak);
            if resp.clicked() {
                self.search.clear();
            }
        }
    }

    // ───────────── 任务列表 ─────────────
    fn list(&mut self, ui: &mut Ui) {
        let p = theme::pal(ui);
        let tasks = self.visible_tasks();
        ui.horizontal(|ui| {
            ui.label(RichText::new(self.filter.label()).font(bold(17.0)).color(p.text));
            ui.label(RichText::new(format!("{} 项", tasks.len())).size(13.0).color(p.weak));
            if self.filter == Filter::Done && !tasks.is_empty() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(ui, Btn::Secondary, Some(Icon::Trash), "清除记录").on_hover_text("仅移除列表记录，不删除文件").clicked() {
                        let ids: Vec<u64> = tasks.iter().map(|t| t.id).collect();
                        self.m.remove(&ids, false);
                    }
                });
            }
        });
        ui.add_space(6.0);
        if tasks.is_empty() {
            self.empty_state(ui, &p);
            return;
        }
        let mods = ui.input(|i| i.modifiers);
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, ROW_H, tasks.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let t = &tasks[i];
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let card = Rect::from_min_max(rect.min + vec2(0.0, 3.0), rect.max - vec2(0.0, 3.0));
                let selected = self.sel.contains(&t.id);
                let hovered = ui.rect_contains_pointer(card);
                self.paint_row(ui, &p, card, t, selected, hovered);
                if hovered {
                    self.row_actions(ui, &p, card, t);
                }
                if resp.clicked() {
                    if mods.shift
                        && let Some(a) = self.anchor
                    {
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

    fn paint_row(&self, ui: &Ui, p: &Pal, card: Rect, t: &Task, selected: bool, hovered: bool) {
        let painter = ui.painter();
        let (fill, stroke) = if selected {
            (p.accent_soft, Stroke::new(1.0, p.accent.gamma_multiply(0.7)))
        } else if hovered {
            (p.panel, Stroke::new(1.0, p.weak.gamma_multiply(0.45)))
        } else {
            (p.panel, Stroke::new(1.0, p.stroke))
        };
        painter.rect(card, 10, p.panel, Stroke::NONE, StrokeKind::Inside);
        painter.rect(card, 10, fill, stroke, StrokeKind::Inside);

        let st = t.status();
        let name = display_name(t);
        let size = t.meta.lock().unwrap().size;
        let done = t.downloaded.load(Relaxed);
        let speed = t.speed.load(Relaxed);
        let sc = status_color(p, st);

        let badge = Rect::from_min_size(pos2(card.min.x + 12.0, card.center().y - 19.0), vec2(38.0, 38.0));
        file_badge(ui, badge, &name);
        let x0 = badge.max.x + 14.0;
        let xr = card.max.x - 16.0;
        let y1 = card.min.y + 20.0;
        let y2 = card.min.y + 41.0;

        // 第一行：文件名 + 右侧速度/状态
        let right1 = if st == Status::Running {
            let txt = if speed > 0 { fmt_speed(speed) } else { "连接中".into() };
            let g = painter.layout_no_wrap(txt, bold(13.5), p.text);
            let w = g.size().x;
            painter.galley(pos2(xr - w, y1 - g.size().y / 2.0), g, p.text);
            w
        } else {
            let g = painter.layout_no_wrap(st.label().into(), prop(13.0), sc);
            let w = g.size().x;
            painter.galley(pos2(xr - w, y1 - g.size().y / 2.0), g, sc);
            icons::paint(painter, Rect::from_center_size(pos2(xr - w - 11.0, y1), vec2(14.0, 14.0)), status_icon(st), sc);
            w + 20.0
        };
        let name_clip = Rect::from_min_max(pos2(x0, card.min.y), pos2(xr - right1 - 16.0, card.max.y));
        let g = painter.layout_no_wrap(name, prop(14.5), p.text);
        let gh = g.size().y;
        painter.with_clip_rect(name_clip).galley(pos2(x0, y1 - gh / 2.0), g, p.text);

        // 第二行：进度条 + 百分比 + 大小 + 右侧剩余时间/完成时间/错误
        let frac = match (st, size) {
            (Status::Done, _) => 1.0,
            (_, Some(s)) if s > 0 => done as f32 / s as f32,
            _ => 0.0,
        };
        let bw = ((xr - x0) * 0.42).clamp(80.0, 340.0);
        let bar = Rect::from_min_size(pos2(x0, y2 - 2.5), vec2(bw, 5.0));
        let bar_c = match st {
            Status::Running => p.accent,
            Status::Done => p.success,
            Status::Error => p.danger,
            _ => p.weak.gamma_multiply(0.7),
        };
        progress(ui, bar, frac, bar_c);
        let info = match (st, size) {
            (Status::Done, Some(s)) => fmt_size(s),
            (_, Some(s)) => format!("{:.1}%    {} / {}", frac * 100.0, fmt_size(done), fmt_size(s)),
            (_, None) if done > 0 => fmt_size(done),
            _ => String::new(),
        };
        painter.text(pos2(bar.max.x + 12.0, y2), Align2::LEFT_CENTER, info, prop(12.5), p.weak);
        let right2 = match st {
            Status::Running => size
                .filter(|_| speed > 0)
                .map(|s| format!("剩余 {}", fmt_eta(s.saturating_sub(done) / speed)))
                .unwrap_or_default(),
            Status::Done => fmt_ago(t.meta.lock().unwrap().finished),
            Status::Error => t.meta.lock().unwrap().error.clone().map(|e| e.chars().take(28).collect()).unwrap_or_default(),
            _ => String::new(),
        };
        let c2 = if st == Status::Error { p.danger } else { p.weak };
        painter.text(pos2(xr, y2), Align2::RIGHT_CENTER, right2, prop(12.5), c2);
    }

    /// 悬停时显示的行内操作按钮
    fn row_actions(&mut self, ui: &mut Ui, p: &Pal, card: Rect, t: &Arc<Task>) {
        let st = t.status();
        let (main_icon, main_tip) = match st {
            Status::Running | Status::Queued => (Icon::Pause, "暂停"),
            Status::Done => (Icon::External, "打开文件"),
            _ => (Icon::Play, "开始"),
        };
        let actions = [(main_icon, main_tip), (Icon::Folder, "打开所在文件夹"), (Icon::Trash, "删除")];
        let bw = 30.0;
        let total = bw * actions.len() as f32 + 4.0 * (actions.len() - 1) as f32;
        let area = Rect::from_min_max(pos2(card.max.x - total - 24.0, card.min.y + 1.0), pos2(card.max.x - 1.0, card.max.y - 1.0));
        let bg = if self.sel.contains(&t.id) { p.accent_soft } else { p.panel };
        let radius = egui::CornerRadius { nw: 0, sw: 0, ne: 9, se: 9 };
        ui.painter().rect_filled(area, radius, p.panel);
        ui.painter().rect_filled(area, radius, bg);
        let mut x = card.max.x - 12.0 - total;
        for (i, (icon, tip)) in actions.into_iter().enumerate() {
            let r = Rect::from_min_size(pos2(x, card.center().y - bw / 2.0), vec2(bw, bw));
            x += bw + 4.0;
            let resp = ui.interact(r, Id::unique(("row_act", t.id, i)), Sense::click()).on_hover_text(tip);
            if resp.hovered() {
                ui.painter().rect_filled(r, 8, p.hover);
            }
            let c = if i == 2 && resp.hovered() { p.danger } else { p.text };
            icons::paint(ui.painter(), Rect::from_center_size(r.center(), vec2(16.0, 16.0)), icon, c);
            if resp.clicked() {
                match i {
                    0 => match st {
                        Status::Running | Status::Queued => self.m.pause(t.id),
                        Status::Done => open_path(&t.final_path(), false),
                        _ => self.m.start(t.id),
                    },
                    1 => open_path(&if st == Status::Done { t.final_path() } else { t.part_path() }, true),
                    _ => self.delete = Some(DeleteDialog::new(vec![t.id])),
                }
            }
        }
    }

    fn row_menu(&mut self, ui: &mut Ui, t: &Arc<Task>) {
        let st = t.status();
        if st == Status::Done && menu_item(ui, Icon::External, "打开文件", false).clicked() {
            open_path(&t.final_path(), false);
            ui.close();
        }
        if menu_item(ui, Icon::Folder, "打开所在文件夹", false).clicked() {
            open_path(&if st == Status::Done { t.final_path() } else { t.part_path() }, true);
            ui.close();
        }
        ui.separator();
        if matches!(st, Status::Paused | Status::Error) && menu_item(ui, Icon::Play, "开始", false).clicked() {
            self.selected().into_iter().for_each(|id| self.m.start(id));
            ui.close();
        }
        if matches!(st, Status::Running | Status::Queued) && menu_item(ui, Icon::Pause, "暂停", false).clicked() {
            self.selected().into_iter().for_each(|id| self.m.pause(id));
            ui.close();
        }
        if st != Status::Running && menu_item(ui, Icon::Refresh, "重新下载", false).clicked() {
            self.m.restart(t.id);
            ui.close();
        }
        if menu_item(ui, Icon::Copy, "复制链接", false).clicked() {
            *self.m.clip_ignore.lock().unwrap() = t.req.url.clone();
            ui.ctx().copy_text(t.req.url.clone());
            ui.close();
        }
        ui.separator();
        if menu_item(ui, Icon::Trash, "删除", true).clicked() {
            self.delete = Some(DeleteDialog::new(self.selected()));
            ui.close();
        }
    }

    fn empty_state(&mut self, ui: &mut Ui, p: &Pal) {
        let filtered = self.filter != Filter::All || !self.search.trim().is_empty();
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.22).max(20.0));
            let (r, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 40.0, p.hover);
            icons::paint(ui.painter(), r.shrink(12.0), if filtered { Icon::Search } else { Icon::Inbox }, p.weak);
            ui.add_space(18.0);
            let (title, sub) = if filtered {
                ("没有符合条件的任务", "试试其他分类或搜索词")
            } else {
                ("暂无下载任务", "复制下载链接、在此按 Ctrl+V 粘贴，或使用浏览器扩展")
            };
            ui.label(RichText::new(title).font(bold(16.0)).color(p.text));
            ui.label(RichText::new(sub).size(13.0).color(p.weak));
            if !filtered {
                ui.add_space(14.0);
                if button(ui, Btn::Primary, Some(Icon::Plus), "新建下载").clicked() {
                    self.open_add(ui.ctx(), vec![]);
                }
            }
        });
    }

    // ───────────── 详情抽屉 ─────────────
    fn details(&self, ui: &mut Ui, p: &Pal, t: &Arc<Task>) {
        let (dir, size, err, resumable, probed, created) = {
            let m = t.meta.lock().unwrap();
            (m.dir.clone(), m.size, m.error.clone(), m.resumable, m.probed, m.created)
        };
        let st = t.status();
        let name = display_name(t);
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
            file_badge(ui, r, &name);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.set_max_width(ui.available_width() - 120.0);
                ui.add(egui::Label::new(RichText::new(&name).font(bold(15.0)).color(p.text)).truncate());
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                    icons::paint(ui.painter(), r, status_icon(st), status_color(p, st));
                    ui.label(RichText::new(st.label()).size(12.5).color(status_color(p, st)));
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, Icon::Copy, "复制链接", true).clicked() {
                    *self.m.clip_ignore.lock().unwrap() = t.req.url.clone();
                    ui.ctx().copy_text(t.req.url.clone());
                }
                let folder = if st == Status::Done { t.final_path() } else { t.part_path() };
                if icon_button(ui, Icon::Folder, "打开所在文件夹", true).clicked() {
                    open_path(&folder, true);
                }
                if icon_button(ui, Icon::External, "打开文件", st == Status::Done).clicked() {
                    open_path(&t.final_path(), false);
                }
            });
        });
        ui.add_space(8.0);
        segment_bar(ui, t);
        ui.add_space(8.0);
        let kv = |ui: &mut Ui, k: &str, v: String| {
            ui.horizontal(|ui| {
                ui.add_sized([64.0, 18.0], egui::Label::new(RichText::new(k).size(12.5).color(p.weak)));
                ui.add(egui::Label::new(RichText::new(v).size(13.0).color(p.text)).truncate());
            });
        };
        ui.columns(2, |c| {
            c[0].spacing_mut().item_spacing.y = 4.0;
            c[1].spacing_mut().item_spacing.y = 4.0;
            kv(&mut c[0], "保存位置", dir.display().to_string());
            kv(&mut c[0], "下载链接", t.req.url.clone());
            kv(&mut c[0], "添加时间", fmt_ago(created));
            let total = size.map(fmt_size).unwrap_or_else(|| "未知".into());
            kv(&mut c[1], "进度", format!("{} / {}", fmt_size(t.downloaded.load(Relaxed)), total));
            kv(&mut c[1], "连接数", format!("{} / {}", t.active_conns.load(Relaxed), t.conns));
            kv(&mut c[1], "断点续传", if !probed { "未知" } else if resumable { "支持" } else { "不支持" }.into());
        });
        if let Some(e) = err {
            ui.add_space(4.0);
            Frame::new().fill(p.danger.gamma_multiply(0.12)).corner_radius(8).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                    icons::paint(ui.painter(), r, Icon::Alert, p.danger);
                    ui.add(egui::Label::new(RichText::new(e).size(13.0).color(p.danger)).truncate());
                });
            });
        }
    }

    // ───────────── 提示 ─────────────
    fn toasts(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.born.elapsed() < Duration::from_millis(4000));
        if self.toasts.is_empty() {
            return;
        }
        let p = theme::pal_ctx(ctx);
        let shadow = ctx.global_style().visuals.popup_shadow;
        let mut open = None;
        let mut y = -16.0;
        for t in self.toasts.iter().rev() {
            let age = t.born.elapsed().as_secs_f32();
            let alpha = (age / 0.15).min((4.0 - age) / 0.5).clamp(0.0, 1.0);
            let resp = egui::Area::new(Id::unique(("toast", t.task, t.born)))
                .anchor(Align2::RIGHT_BOTTOM, vec2(-16.0, y))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    ui.set_opacity(alpha);
                    Frame::new()
                        .fill(p.panel)
                        .stroke(Stroke::new(1.0, p.stroke))
                        .corner_radius(10)
                        .shadow(shadow)
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            ui.set_width(300.0);
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                                let (icon, c, title) =
                                    if t.ok { (Icon::Check, p.success, "下载完成") } else { (Icon::Alert, p.danger, "下载失败") };
                                icons::paint(ui.painter(), r, icon, c);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 1.0;
                                    ui.label(RichText::new(title).font(bold(14.0)).color(p.text));
                                    ui.add(egui::Label::new(RichText::new(&t.name).size(12.5).color(p.weak)).truncate());
                                });
                            });
                        })
                })
                .response;
            if t.ok && resp.interact(Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                open = Some(t.task);
            }
            y -= resp.rect.height() + 10.0;
        }
        if let Some(id) = open
            && let Some(t) = self.m.get(id)
        {
            open_path(&t.final_path(), false);
            self.toasts.retain(|x| x.task != id);
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.add.is_some() || self.settings.is_some() || self.delete.is_some() {
            return;
        }
        let typing = ctx.memory(|m| m.focused().is_some());
        let (new, find, del, all, space, paste) = ctx.input(|i| {
            let paste = i.events.iter().find_map(|e| match e {
                egui::Event::Paste(s) => Some(s.clone()),
                _ => None,
            });
            (
                i.modifiers.command && i.key_pressed(Key::N),
                i.modifiers.command && i.key_pressed(Key::F),
                i.key_pressed(Key::Delete),
                i.modifiers.command && i.key_pressed(Key::A),
                i.key_pressed(Key::Space),
                paste,
            )
        });
        if new {
            self.open_add(ctx, vec![]);
        }
        if find {
            self.focus_search = true;
        }
        if typing {
            return; // 输入框内的按键不触发列表快捷键
        }
        if del && !self.sel.is_empty() {
            self.delete = Some(DeleteDialog::new(self.selected()));
        }
        if all {
            self.sel = self.visible_tasks().iter().map(|t| t.id).collect();
        }
        if space {
            for id in self.selected() {
                match self.m.get(id).map(|t| t.status()) {
                    Some(Status::Running | Status::Queued) => self.m.pause(id),
                    _ => self.m.start(id),
                }
            }
        }
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
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mode = self.m.settings.read().unwrap().theme;
        if self.theme != Some(mode) {
            theme::apply_mode(&ctx, mode);
            self.theme = Some(mode);
        }
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                UiEvent::Add(r) => self.open_add(&ctx, r),
                UiEvent::Show => bring_to_front(&ctx),
            }
        }
        self.shortcuts(&ctx);
        self.sample_speed();
        self.detect_changes();
        let p = theme::pal(ui);

        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(212.0)
            .frame(Frame::new().fill(p.side).inner_margin(Margin { left: 14, right: 14, top: 16, bottom: 14 }))
            .show(ui, |ui| self.sidebar(ui));
        egui::Panel::top("topbar")
            .frame(Frame::new().fill(p.panel).inner_margin(Margin::symmetric(16, 10)))
            .show(ui, |ui| self.topbar(ui));
        let detail = (self.sel.len() == 1).then(|| self.sel.iter().next().and_then(|&id| self.m.get(id))).flatten();
        if let Some(t) = &detail {
            egui::Panel::bottom("detail")
                .resizable(false)
                .frame(Frame::new().fill(p.panel).inner_margin(Margin::symmetric(18, 14)))
                .show(ui, |ui| self.details(ui, &p, t));
        }
        egui::CentralPanel::no_frame()
            .frame(Frame::new().fill(p.bg).inner_margin(Margin { left: 18, right: 18, top: 14, bottom: 6 }))
            .show(ui, |ui| self.list(ui));

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
        self.toasts(&ctx);

        // 仅在有任务活动时定时刷新，空闲时零 CPU
        if self.m.tasks.read().unwrap().iter().any(|t| t.status() == Status::Running) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

fn nav_item(ui: &mut Ui, p: &Pal, icon: Icon, label: &str, count: usize, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 8, p.accent_soft);
    } else if resp.hovered() {
        painter.rect_filled(rect, 8, p.hover);
    }
    let c = if selected { p.accent } else { p.text };
    icons::paint(painter, Rect::from_min_size(pos2(rect.min.x + 10.0, rect.center().y - 8.0), vec2(16.0, 16.0)), icon, c);
    let font = if selected { bold(14.0) } else { prop(14.0) };
    painter.text(pos2(rect.min.x + 36.0, rect.center().y), Align2::LEFT_CENTER, label, font, c);
    if count > 0 {
        let c = if selected { p.accent } else { p.weak };
        let g = painter.layout_no_wrap(count.to_string(), prop(12.0), c);
        let pill = Rect::from_center_size(pos2(rect.max.x - 10.0 - (g.size().x + 14.0) / 2.0, rect.center().y), vec2(g.size().x + 14.0, 20.0));
        painter.rect_filled(pill, 10, if selected { p.panel } else { p.track });
        painter.galley(pill.center() - g.size() / 2.0, g, c);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn separator_v(ui: &mut Ui, p: &Pal) {
    let (r, _) = ui.allocate_exact_size(vec2(1.0, 22.0), Sense::hover());
    ui.painter().rect_filled(r, 0, p.stroke);
}

fn display_name(t: &Task) -> String {
    let m = t.meta.lock().unwrap();
    if m.name.is_empty() {
        crate::engine::probe::url_name(&t.req.url).unwrap_or_else(|| t.req.url.clone())
    } else {
        m.name.clone()
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

#[cfg(test)]
mod tests {
    /// 防回退：界面与引擎中显示给用户的字符串只允许 ASCII、汉字和常用中文标点，
    /// 杜绝在部分系统上会显示成方框的特殊符号与 emoji。
    #[test]
    fn ui_strings_only_safe_glyphs() {
        let ok = |c: char| c.is_ascii() || ('\u{4E00}'..='\u{9FFF}').contains(&c) || "，。：（）、！？；“”《》".contains(c);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = ["ui/mod.rs", "ui/dialogs.rs", "ui/widgets.rs", "ui/theme.rs", "ui/icons.rs", "engine/mod.rs", "engine/task.rs", "engine/probe.rs"];
        let mut bad = vec![];
        for f in files {
            let src = std::fs::read_to_string(root.join(f)).unwrap();
            for (n, line) in src.lines().enumerate() {
                // 只检查代码部分：去掉行尾 // 注释（跳过 "https://" 里的 //）
                let code = match line.find(" //") {
                    Some(i) => &line[..i],
                    None => line,
                };
                if code.trim_start().starts_with("//") {
                    continue;
                }
                if let Some(c) = code.chars().find(|&c| !ok(c)) {
                    bad.push(format!("{f}:{} 含不安全字符 {c:?}", n + 1));
                }
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}
