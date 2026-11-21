mod dialogs;
mod icons;
mod theme;
mod widgets;

pub use theme::app_icon;

use crate::engine::{AddReq, Manager, Move, Status, Task, ThemeMode, UiEvent};
use crate::tray::{Tray, TrayCmd};
use dialogs::{AddDialog, CloseAsk, CloseChoice, DeleteDialog, DialogResult, ExtGuide, RelinkDialog, SettingsDialog, status_text};
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
const COUNTDOWN: u64 = 60;

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
            Self::All => tr!("全部任务"),
            Self::Running => tr!("下载中"),
            Self::Queued => tr!("排队中"),
            Self::Unfinished => tr!("未完成"),
            Self::Done => tr!("已完成"),
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Added,
    Name,
    Size,
    Status,
}

impl Sort {
    const ALL: [Sort; 4] = [Self::Added, Self::Name, Self::Size, Self::Status];
    fn label(self) -> &'static str {
        match self {
            Self::Added => tr!("队列顺序"),
            Self::Name => tr!("名称"),
            Self::Size => tr!("大小"),
            Self::Status => tr!("状态"),
        }
    }
}

/// 全部完成后的动作（仅本次运行有效）
#[derive(Clone, Copy, PartialEq, Eq)]
enum AfterAll {
    Nothing,
    Exit,
    Sleep,
    Shutdown,
}

impl AfterAll {
    const ALL: [AfterAll; 4] = [Self::Nothing, Self::Exit, Self::Sleep, Self::Shutdown];
    fn label(self) -> &'static str {
        match self {
            Self::Nothing => tr!("不执行操作"),
            Self::Exit => tr!("退出程序"),
            Self::Sleep => tr!("睡眠"),
            Self::Shutdown => tr!("关机"),
        }
    }
    fn verb(self) -> &'static str {
        match self {
            Self::Nothing => "",
            Self::Exit => tr!("退出 SwiftDL"),
            Self::Sleep => tr!("进入睡眠"),
            Self::Shutdown => tr!("关闭计算机"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Ok,
    Err,
}

type UpdateSlot = Arc<std::sync::Mutex<Option<Result<Option<crate::update::Release>, String>>>>;

/// 更新下载状态
#[derive(Clone, Default)]
enum UpdateJob {
    #[default]
    Idle,
    Downloading,
    Ready(std::path::PathBuf),
    Failed(String),
}

struct Toast {
    task: Option<u64>,
    kind: ToastKind,
    title: &'static str,
    text: String,
    born: Instant,
}

/// 每帧一次的任务快照：侧栏计数、过滤、排序、绘制共用，避免重复加锁
struct Row {
    t: Arc<Task>,
    status: Status,
    name: String,
    size: Option<u64>,
    done: u64,
    speed: u64,
    scheduled: bool,
    finished: u64,
    error: Option<String>,
    /// HLS：(已完成分片, 分片总数)，非 HLS 为 None
    segs: Option<(u32, u32)>,
}

pub struct App {
    m: Arc<Manager>,
    rx: Receiver<UiEvent>,
    sel: HashSet<u64>,
    anchor: Option<usize>,
    add: Option<AddDialog>,
    settings: Option<SettingsDialog>,
    delete: Option<DeleteDialog>,
    relink: Option<RelinkDialog>,
    ext_guide: Option<ExtGuide>,
    close_ask: Option<CloseAsk>,
    filter: Filter,
    sort: Sort,
    sort_desc: bool,
    search: String,
    focus_search: bool,
    history: VecDeque<u64>,
    last_sample: Instant,
    prev: HashMap<u64, Status>,
    toasts: Vec<Toast>,
    theme: Option<ThemeMode>,
    tray: Option<Tray>,
    notifier: crate::winsys::Notifier,
    taskbar: Option<crate::winsys::Taskbar>,
    drop_over: Option<Arc<std::sync::atomic::AtomicBool>>,
    hidden: bool,
    tray_tip: (String, Instant),
    /// 自动更新：检查结果、下载状态
    update: Option<crate::update::Release>,
    update_slot: UpdateSlot,
    update_job: Arc<std::sync::Mutex<UpdateJob>>,
    update_checked_at: Instant,
    quitting: bool,
    start_hidden: bool,
    first_frame: bool,
    after_all: AfterAll,
    was_busy: bool,
    countdown_at: Option<Instant>,
    ext_was_connected: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, m: Arc<Manager>, rx: Receiver<UiEvent>, tx: std::sync::mpsc::Sender<UiEvent>, minimized: bool) -> Self {
        theme::install(&cc.egui_ctx);
        let _ = m.ui_ctx.set(cc.egui_ctx.clone());
        let tray = crate::tray::create(&cc.egui_ctx);
        let hwnd = crate::winsys::hwnd_of(cc);
        if let Some(h) = hwnd {
            crate::winsys::set_main_hwnd(h);
        }
        let taskbar = hwnd.and_then(crate::winsys::Taskbar::new);
        let drop_over = hwnd.and_then(|h| crate::winsys::register_drop(h, tx, cc.egui_ctx.clone()));
        Self {
            m,
            rx,
            sel: HashSet::new(),
            anchor: None,
            add: None,
            settings: None,
            delete: None,
            relink: None,
            ext_guide: None,
            close_ask: None,
            filter: Filter::All,
            sort: Sort::Added,
            sort_desc: false,
            search: String::new(),
            focus_search: false,
            history: VecDeque::with_capacity(HISTORY + 1),
            last_sample: Instant::now(),
            prev: HashMap::new(),
            toasts: Vec::new(),
            theme: None,
            start_hidden: minimized && tray.is_some(),
            tray,
            quitting: false,
            first_frame: true,
            after_all: AfterAll::Nothing,
            was_busy: false,
            countdown_at: None,
            ext_was_connected: false,
            notifier: crate::winsys::Notifier::new(),
            taskbar,
            drop_over,
            hidden: false,
            tray_tip: (String::new(), Instant::now()),
            update: None,
            update_slot: Arc::default(),
            update_job: Arc::default(),
            update_checked_at: Instant::now() - Duration::from_secs(3600),
        }
    }

    fn open_add(&mut self, ctx: &egui::Context, reqs: Vec<AddReq>) {
        match &mut self.add {
            Some(d) => d.append(reqs),
            None => self.add = Some(AddDialog::new(&self.m, reqs)),
        }
        self.show(ctx);
    }

    fn toast(&mut self, task: Option<u64>, kind: ToastKind, title: &'static str, text: String) {
        self.toasts.push(Toast { task, kind, title, text, born: Instant::now() });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn selected(&self) -> Vec<u64> {
        self.sel.iter().copied().collect()
    }

    fn all_ids(&self) -> Vec<u64> {
        self.m.tasks.read().unwrap().iter().map(|t| t.id).collect()
    }

    fn snapshot(&self) -> Vec<Row> {
        self.m
            .tasks
            .read()
            .unwrap()
            .iter()
            .map(|t| {
                let m = t.meta.lock().unwrap();
                let name = if m.name.is_empty() {
                    crate::engine::probe::url_name(&t.url()).unwrap_or_else(|| t.url())
                } else {
                    m.name.clone()
                };
                Row {
                    t: t.clone(),
                    status: t.status(),
                    name,
                    size: m.size,
                    done: t.downloaded.load(Relaxed),
                    speed: t.speed.load(Relaxed),
                    scheduled: m.scheduled,
                    finished: m.finished,
                    error: m.error.clone(),
                    segs: m.hls.is_some().then(|| (t.seg_done.load(Relaxed), t.seg_total.load(Relaxed))).filter(|s| s.1 > 0),
                }
            })
            .collect()
    }

    /// 过滤 + 搜索 + 排序
    fn visible<'a>(&self, rows: &'a [Row]) -> Vec<&'a Row> {
        let q = self.search.trim().to_lowercase();
        let mut v: Vec<&Row> = rows
            .iter()
            .filter(|r| self.filter.matches(r.status))
            .filter(|r| q.is_empty() || r.name.to_lowercase().contains(&q) || r.t.url().to_lowercase().contains(&q))
            .collect();
        match self.sort {
            Sort::Added => {}
            Sort::Name => v.sort_by_cached_key(|r| r.name.to_lowercase()),
            Sort::Size => v.sort_by_key(|r| r.size.unwrap_or(0)),
            Sort::Status => v.sort_by_key(|r| r.status as u8),
        }
        if self.sort_desc {
            v.reverse();
        }
        v
    }

    /// 每 500ms 采样总速度，供侧栏折线使用
    fn sample_speed(&mut self, rows: &[Row]) {
        let now = Instant::now();
        if now.duration_since(self.last_sample) > Duration::from_secs(60) {
            self.history.clear();
            self.last_sample = now;
        }
        let total: u64 = rows.iter().map(|r| r.speed).sum();
        while now.duration_since(self.last_sample) >= SAMPLE {
            self.last_sample += SAMPLE;
            self.history.push_back(total);
            if self.history.len() > HISTORY {
                self.history.pop_front();
            }
        }
    }

    fn show(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        show_window(ctx);
    }

    fn hide(&mut self, ctx: &egui::Context) {
        self.hidden = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    /// 任务状态变化时提示：窗口在前台用应用内提示，隐藏/不在前台时用系统通知
    fn detect_changes(&mut self, ctx: &egui::Context, rows: &[Row]) {
        let foreground = !self.hidden && ctx.input(|i| i.viewport().focused.unwrap_or(true));
        for r in rows {
            if let Some(prev) = self.prev.insert(r.t.id, r.status)
                && prev != r.status
            {
                let (kind, title, text) = match r.status {
                    Status::Done => (ToastKind::Ok, tr!("下载完成"), r.name.clone()),
                    Status::Error if prev == Status::Running => {
                        (ToastKind::Err, tr!("下载失败"), trf!("{}：{}", r.name, r.error.clone().unwrap_or_default()))
                    }
                    _ => continue,
                };
                if foreground {
                    self.toast(Some(r.t.id), kind, title, text);
                } else {
                    self.notifier.send(title, &text);
                }
            }
        }
        if self.prev.len() > rows.len() {
            self.prev.retain(|id, _| rows.iter().any(|r| r.t.id == *id));
            self.sel.retain(|id| rows.iter().any(|r| r.t.id == *id));
        }
    }

    // ───────────── 侧边栏 ─────────────
    fn sidebar(&mut self, ui: &mut Ui, rows: &[Row]) {
        let p = theme::pal(ui);
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
            logo(ui, r);
            ui.add_space(2.0);
            ui.label(RichText::new("SwiftDL").font(bold(18.0)).color(p.text));
        });
        ui.add_space(22.0);
        ui.label(RichText::new(tr!("分类")).size(12.0).color(p.weak));
        ui.add_space(2.0);
        ui.spacing_mut().item_spacing.y = 2.0;
        for f in Filter::ALL {
            let n = rows.iter().filter(|r| f.matches(r.status)).count();
            if nav_item(ui, &p, f.icon(), f.label(), n, self.filter == f).clicked() {
                self.filter = f;
            }
        }

        // 底部：完成动作 + 速度卡片
        let limit = self.m.settings.read().unwrap().speed_limit_kb;
        let card_h = if limit > 0 { 132.0 } else { 114.0 };
        let after_h = 58.0;
        let gap = ui.available_height() - card_h - after_h;
        if gap > 0.0 {
            ui.add_space(gap);
        }
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(RichText::new(tr!("全部完成后")).size(12.0).color(p.weak));
        egui::ComboBox::from_id_salt("after_all")
            .width(ui.available_width())
            .selected_text(self.after_all.label())
            .show_ui(ui, |ui| {
                for a in AfterAll::ALL {
                    ui.selectable_value(&mut self.after_all, a, a.label());
                }
            });
        ui.add_space(8.0);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), card_h), Sense::hover());
        let painter = ui.painter();
        painter.rect(rect, 10, p.panel, Stroke::new(1.0, p.stroke), StrokeKind::Inside);
        let inner = rect.shrink(12.0);
        icons::paint(painter, Rect::from_min_size(inner.min, vec2(14.0, 14.0)), Icon::ArrowDown, p.accent);
        painter.text(pos2(inner.min.x + 20.0, inner.min.y + 7.0), Align2::LEFT_CENTER, tr!("总速度"), prop(12.0), p.weak);
        let total = self.history.back().copied().unwrap_or(0);
        painter.text(pos2(inner.min.x, inner.min.y + 32.0), Align2::LEFT_CENTER, fmt_speed(total), bold(19.0), p.text);
        let spark = Rect::from_min_size(pos2(inner.min.x, inner.min.y + 48.0), vec2(inner.width(), 40.0));
        sparkline(ui, spark, &self.history, p.accent);
        if limit > 0 {
            ui.painter().text(
                pos2(inner.min.x, inner.max.y - 6.0),
                Align2::LEFT_CENTER,
                trf!("已限速 {}", fmt_speed(limit * 1024)),
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
            if button(ui, Btn::Primary, Some(Icon::Plus), tr!("新建下载")).on_hover_text("Ctrl+N").clicked() {
                self.open_add(ui.ctx(), vec![]);
            }
            ui.add_space(10.0);
            let has = !self.sel.is_empty();
            if icon_button(ui, Icon::Play, tr!("开始（Space）"), has).clicked() {
                self.selected().into_iter().for_each(|id| self.m.start(id));
            }
            if icon_button(ui, Icon::Pause, tr!("暂停（Space）"), has).clicked() {
                self.selected().into_iter().for_each(|id| self.m.pause(id));
            }
            if icon_button(ui, Icon::Trash, tr!("删除（Delete）"), has).clicked() {
                self.delete = Some(DeleteDialog::new(self.selected()));
            }
            ui.add_space(6.0);
            separator_v(ui, &p);
            ui.add_space(6.0);
            if button(ui, Btn::Secondary, Some(Icon::Play), tr!("全部开始")).clicked() {
                self.all_ids().into_iter().for_each(|id| self.m.start(id));
            }
            ui.add_space(4.0);
            if button(ui, Btn::Secondary, Some(Icon::Pause), tr!("全部暂停")).clicked() {
                self.all_ids().into_iter().for_each(|id| self.m.pause(id));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, Icon::Settings, tr!("设置"), true).clicked() {
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
            egui::TextEdit::singleline(&mut self.search).id(id).frame(Frame::NONE).hint_text(tr!("搜索文件名或链接")),
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
    fn list(&mut self, ui: &mut Ui, rows: &[Row]) {
        let p = theme::pal(ui);
        let vis = self.visible(rows);
        self.update_banner(ui, &p);
        ui.horizontal(|ui| {
            ui.label(RichText::new(self.filter.label()).font(bold(17.0)).color(p.text));
            ui.label(RichText::new(trf!("{} 项", vis.len())).size(13.0).color(p.weak));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (icon, tip) = if self.sort_desc { (Icon::ArrowDown, tr!("降序")) } else { (Icon::ArrowUp, tr!("升序")) };
                if icon_button(ui, icon, tip, true).clicked() {
                    self.sort_desc = !self.sort_desc;
                }
                egui::ComboBox::from_id_salt("sort").width(96.0).selected_text(self.sort.label()).show_ui(ui, |ui| {
                    for s in Sort::ALL {
                        ui.selectable_value(&mut self.sort, s, s.label());
                    }
                });
                ui.label(RichText::new(tr!("排序")).size(13.0).color(p.weak));
                if self.filter == Filter::Done && !vis.is_empty() {
                    ui.add_space(8.0);
                    if button(ui, Btn::Secondary, Some(Icon::Trash), tr!("清除记录")).on_hover_text(tr!("仅移除列表记录，不删除文件")).clicked() {
                        let ids: Vec<u64> = vis.iter().map(|r| r.t.id).collect();
                        self.m.remove(&ids, false);
                    }
                }
            });
        });
        ui.add_space(6.0);
        if vis.is_empty() {
            self.empty_state(ui, &p);
            return;
        }
        let mods = ui.input(|i| i.modifiers);
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, ROW_H, vis.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let r = vis[i];
                let id = r.t.id;
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let card = Rect::from_min_max(rect.min + vec2(0.0, 3.0), rect.max - vec2(0.0, 3.0));
                let selected = self.sel.contains(&id);
                let hovered = ui.rect_contains_pointer(card);
                paint_row(ui, &p, card, r, selected, hovered);
                if hovered {
                    self.row_actions(ui, &p, card, r);
                }
                if resp.clicked() {
                    if mods.shift
                        && let Some(a) = self.anchor
                    {
                        let (lo, hi) = (a.min(i), a.max(i));
                        if !mods.command {
                            self.sel.clear();
                        }
                        self.sel.extend(vis[lo..=hi.min(vis.len() - 1)].iter().map(|r| r.t.id));
                    } else if mods.command {
                        if !self.sel.remove(&id) {
                            self.sel.insert(id);
                        }
                        self.anchor = Some(i);
                    } else {
                        self.sel.clear();
                        self.sel.insert(id);
                        self.anchor = Some(i);
                    }
                }
                if resp.double_clicked() {
                    match r.status {
                        Status::Done => open_path(&r.t.final_path(), false),
                        Status::Running | Status::Queued => self.m.pause(id),
                        _ => self.m.start(id),
                    }
                }
                if resp.secondary_clicked() && !selected {
                    self.sel.clear();
                    self.sel.insert(id);
                    self.anchor = Some(i);
                }
                resp.context_menu(|ui| self.row_menu(ui, r));
            }
        });
    }

    /// 悬停时显示的行内操作按钮
    fn row_actions(&mut self, ui: &mut Ui, p: &Pal, card: Rect, r: &Row) {
        let st = r.status;
        let t = &r.t;
        let (main_icon, main_tip) = match st {
            Status::Running | Status::Queued => (Icon::Pause, tr!("暂停")),
            Status::Done => (Icon::External, tr!("打开文件")),
            _ => (Icon::Play, tr!("开始")),
        };
        let actions = [(main_icon, main_tip), (Icon::Folder, tr!("打开所在文件夹")), (Icon::Trash, tr!("删除"))];
        let bw = 30.0;
        let total = bw * actions.len() as f32 + 4.0 * (actions.len() - 1) as f32;
        let area = Rect::from_min_max(pos2(card.max.x - total - 24.0, card.min.y + 1.0), pos2(card.max.x - 1.0, card.max.y - 1.0));
        let bg = if self.sel.contains(&t.id) { p.accent_soft } else { p.panel };
        let radius = egui::CornerRadius { nw: 0, sw: 0, ne: 9, se: 9 };
        ui.painter().rect_filled(area, radius, p.panel);
        ui.painter().rect_filled(area, radius, bg);
        let mut x = card.max.x - 12.0 - total;
        for (i, (icon, tip)) in actions.into_iter().enumerate() {
            let rr = Rect::from_min_size(pos2(x, card.center().y - bw / 2.0), vec2(bw, bw));
            x += bw + 4.0;
            let resp = ui.interact(rr, Id::unique(("row_act", t.id, i)), Sense::click()).on_hover_text(tip);
            if resp.hovered() {
                ui.painter().rect_filled(rr, 8, p.hover);
            }
            let c = if i == 2 && resp.hovered() { p.danger } else { p.text };
            icons::paint(ui.painter(), Rect::from_center_size(rr.center(), vec2(16.0, 16.0)), icon, c);
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

    fn row_menu(&mut self, ui: &mut Ui, r: &Row) {
        let st = r.status;
        let t = &r.t;
        if st == Status::Done && menu_item(ui, Icon::External, tr!("打开文件"), false).clicked() {
            open_path(&t.final_path(), false);
            ui.close();
        }
        if st == Status::Done && menu_item(ui, Icon::External, tr!("选择打开方式"), false).clicked() {
            crate::winsys::open_file(&t.final_path(), true);
            ui.close();
        }
        if menu_item(ui, Icon::Folder, tr!("打开所在文件夹"), false).clicked() {
            open_path(&if st == Status::Done { t.final_path() } else { t.part_path() }, true);
            ui.close();
        }
        ui.separator();
        if matches!(st, Status::Paused | Status::Error) && menu_item(ui, Icon::Play, tr!("开始"), false).clicked() {
            self.selected().into_iter().for_each(|id| self.m.start(id));
            ui.close();
        }
        if st == Status::Error
            && r.error.as_deref().is_some_and(crate::engine::sys::is_cert_error)
            && !crate::engine::is_trusted_url(&t.url())
            && menu_item(ui, Icon::Refresh, tr!("信任此站点并重试"), false).clicked()
        {
            self.m.trust_host(&t.url());
            self.m.start(t.id);
            ui.close();
        }
        if matches!(st, Status::Running | Status::Queued) && menu_item(ui, Icon::Pause, tr!("暂停"), false).clicked() {
            self.selected().into_iter().for_each(|id| self.m.pause(id));
            ui.close();
        }
        if matches!(st, Status::Queued | Status::Paused) {
            if menu_item(ui, Icon::ArrowUp, tr!("移到队首"), false).clicked() {
                self.m.move_task(t.id, Move::Top);
                ui.close();
            }
            if menu_item(ui, Icon::ArrowUp, tr!("上移"), false).clicked() {
                self.m.move_task(t.id, Move::Up);
                ui.close();
            }
            if menu_item(ui, Icon::ArrowDown, tr!("下移"), false).clicked() {
                self.m.move_task(t.id, Move::Down);
                ui.close();
            }
        }
        ui.separator();
        if st != Status::Done && menu_item(ui, Icon::Refresh, tr!("更新下载链接"), false).clicked() {
            self.relink = RelinkDialog::new(&self.m, t.id);
            ui.close();
        }
        if st != Status::Running && menu_item(ui, Icon::Refresh, tr!("重新下载"), false).clicked() {
            self.m.restart(t.id);
            ui.close();
        }
        if st == Status::Done && menu_item(ui, Icon::Check, tr!("计算 SHA-256"), false).clicked() {
            self.m.hash_task(t.id);
            self.sel.clear();
            self.sel.insert(t.id);
            ui.close();
        }
        if menu_item(ui, Icon::Copy, tr!("复制链接"), false).clicked() {
            let url = t.url();
            *self.m.clip_ignore.lock().unwrap() = url.clone();
            ui.ctx().copy_text(url);
            ui.close();
        }
        ui.separator();
        if menu_item(ui, Icon::Trash, tr!("删除"), true).clicked() {
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
                (tr!("没有符合条件的任务"), tr!("试试其他分类或搜索词"))
            } else {
                (tr!("暂无下载任务"), tr!("复制下载链接、在此按 Ctrl+V 粘贴，或使用浏览器扩展"))
            };
            ui.label(RichText::new(title).font(bold(16.0)).color(p.text));
            ui.label(RichText::new(sub).size(13.0).color(p.weak));
            if !filtered {
                ui.add_space(14.0);
                if button(ui, Btn::Primary, Some(Icon::Plus), tr!("新建下载")).clicked() {
                    self.open_add(ui.ctx(), vec![]);
                }
            }
        });
    }

    // ───────────── 详情抽屉 ─────────────
    fn details(&self, ui: &mut Ui, p: &Pal, t: &Arc<Task>) {
        let hls = t.meta.lock().unwrap().hls.clone();
        let (name, dir, size, err, resumable, probed, created, scheduled, expect, hash, hashing) = {
            let m = t.meta.lock().unwrap();
            (
                m.name.clone(),
                m.dir.clone(),
                m.size,
                m.error.clone(),
                m.resumable,
                m.probed,
                m.created,
                m.scheduled,
                m.sha256.clone(),
                m.hash.clone(),
                m.hashing,
            )
        };
        let url = t.url();
        let st = t.status();
        let name = if name.is_empty() { crate::engine::probe::url_name(&url).unwrap_or_else(|| url.clone()) } else { name };
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
                    let (icon, c) = if st == Status::Paused && scheduled {
                        (Icon::Clock, p.warning)
                    } else {
                        (status_icon(st), status_color(p, st))
                    };
                    icons::paint(ui.painter(), r, icon, c);
                    ui.label(RichText::new(status_text(st, scheduled)).size(12.5).color(c));
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, Icon::Copy, tr!("复制链接"), true).clicked() {
                    *self.m.clip_ignore.lock().unwrap() = url.clone();
                    ui.ctx().copy_text(url.clone());
                }
                let folder = if st == Status::Done { t.final_path() } else { t.part_path() };
                if icon_button(ui, Icon::Folder, tr!("打开所在文件夹"), true).clicked() {
                    open_path(&folder, true);
                }
                if icon_button(ui, Icon::External, tr!("打开文件"), st == Status::Done).clicked() {
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
            kv(&mut c[0], tr!("保存位置"), dir.display().to_string());
            kv(&mut c[0], tr!("下载链接"), url.clone());
            kv(&mut c[0], tr!("添加时间"), fmt_ago(created));
            let total = size.map(fmt_size).unwrap_or_else(|| tr!("未知").into());
            kv(&mut c[1], tr!("进度"), format!("{} / {}", fmt_size(t.downloaded.load(Relaxed)), total));
            let lim = t.conn_limit.load(Relaxed);
            let conns = if lim < t.conns && st == Status::Running {
                trf!("{} / {}（服务器限流，已降至 {}）", t.active_conns.load(Relaxed), t.conns, lim)
            } else {
                format!("{} / {}", t.active_conns.load(Relaxed), t.conns)
            };
            kv(&mut c[1], tr!("连接数"), conns);
            let bt = t.meta.lock().unwrap().bt.clone();
            if let Some(b) = &bt {
                kv(&mut c[0], tr!("类型"), trf!("BT 任务  {} 个文件", b.file_count));
                kv(&mut c[1], tr!("节点"), trf!("{} 个    已上传 {}", b.peers, fmt_size(b.uploaded)));
            } else if let Some(h) = &hls {
                let mut kind = vec![tr!("HLS 流媒体")];
                if h.encrypted {
                    kind.push(tr!("AES-128 加密"));
                }
                if h.fmp4 {
                    kind.push("fMP4");
                }
                let mins = h.duration as u64 / 60;
                kv(&mut c[0], tr!("类型"), trf!("{}    时长 {}:{}", kind.join("  "), mins, format!("{:02}", h.duration as u64 % 60)));
                kv(&mut c[1], tr!("分片"), format!("{} / {}", t.seg_done.load(Relaxed), h.total));
            } else {
                kv(&mut c[1], tr!("断点续传"), if !probed { tr!("未知") } else if resumable { tr!("支持") } else { tr!("不支持") }.into());
            }
        });
        if let Some(note) = hls.as_ref().and_then(|h| h.note.clone()) {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                icons::paint(ui.painter(), r, Icon::Alert, p.warning);
                ui.label(RichText::new(note).size(12.5).color(p.warning));
            });
        }
        if st == Status::Done || expect.is_some() {
            ui.horizontal(|ui| {
                ui.add_sized([64.0, 22.0], egui::Label::new(RichText::new("SHA-256").size(12.5).color(p.weak)));
                if hashing {
                    ui.add(egui::Spinner::new().size(13.0));
                    ui.label(RichText::new(tr!("正在计算")).size(13.0).color(p.weak));
                } else if let Some(h) = &hash {
                    let matched = expect.as_ref().map(|e| e.eq_ignore_ascii_case(h));
                    let c = match matched {
                        Some(true) => p.success,
                        Some(false) => p.danger,
                        None => p.text,
                    };
                    ui.add(egui::Label::new(RichText::new(h).monospace().size(12.0).color(c)).truncate());
                    if icon_button(ui, Icon::Copy, tr!("复制"), true).clicked() {
                        ui.ctx().copy_text(h.clone());
                    }
                } else if st == Status::Done {
                    if button(ui, Btn::Secondary, Some(Icon::Check), tr!("计算")).clicked() {
                        self.m.hash_task(t.id);
                    }
                } else if let Some(e) = &expect {
                    ui.label(RichText::new(trf!("完成后校验：{}", e)).size(12.5).color(p.weak));
                }
            });
        }
        if let Some(e) = err {
            ui.add_space(4.0);
            Frame::new().fill(p.danger.gamma_multiply(0.12)).corner_radius(8).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                    icons::paint(ui.painter(), r, Icon::Alert, p.danger);
                    if st == Status::Error
                        && crate::engine::sys::is_cert_error(&e)
                        && !crate::engine::is_trusted_url(&url)
                        && let Some(h) = crate::engine::host_key(&url)
                        && button(ui, Btn::Secondary, Some(Icon::Refresh), tr!("信任此站点并重试"))
                            .on_hover_text(trf!("今后忽略 {} 的证书错误。仅在确认该站点可信（如内网、自签名证书）时使用", h))
                            .clicked()
                    {
                        self.m.trust_host(&url);
                        self.m.start(t.id);
                    }
                    ui.add(egui::Label::new(RichText::new(&e).size(13.0).color(p.danger)).truncate()).on_hover_text(&e);
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
                                let (icon, c) = match t.kind {
                                    ToastKind::Ok => (Icon::Check, p.success),
                                    ToastKind::Err => (Icon::Alert, p.danger),
                                };
                                icons::paint(ui.painter(), r, icon, c);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 1.0;
                                    ui.label(RichText::new(t.title).font(bold(14.0)).color(p.text));
                                    ui.add(egui::Label::new(RichText::new(&t.text).size(12.5).color(p.weak)).truncate());
                                });
                            });
                        })
                })
                .response;
            if t.kind == ToastKind::Ok && resp.interact(Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                open = t.task;
            }
            y -= resp.rect.height() + 10.0;
        }
        if let Some(id) = open
            && let Some(t) = self.m.get(id)
        {
            open_path(&t.final_path(), false);
            self.toasts.retain(|x| x.task != Some(id));
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    /// 自动更新：每天检查一次（或手动立即检查），收取结果，下载完成后替换并重启
    fn poll_update(&mut self, ctx: &egui::Context, force: bool) {
        let (auto, last) = {
            let s = self.m.settings.read().unwrap();
            (s.auto_update, s.last_update_check)
        };
        let due = auto && crate::engine::now().saturating_sub(last) >= 86400 && self.update_checked_at.elapsed() >= Duration::from_secs(600);
        if force || due {
            self.update_checked_at = Instant::now();
            let mut s = self.m.settings.read().unwrap().clone();
            s.last_update_check = crate::engine::now();
            self.m.apply_settings(s);
            let (slot, client, m) = (self.update_slot.clone(), self.m.client(), self.m.clone());
            self.m.rt.spawn(async move {
                let r = crate::update::check(&client, crate::update::API).await;
                *slot.lock().unwrap() = Some(r);
                m.repaint();
            });
        }
        let result = self.update_slot.lock().unwrap().take();
        if let Some(r) = result {
            match r {
                Ok(Some(rel)) => self.update = Some(rel),
                Ok(None) if force => self.toast(None, ToastKind::Ok, tr!("已是最新版本"), trf!("当前版本 v{}", env!("CARGO_PKG_VERSION"))),
                Err(e) if force => self.toast(None, ToastKind::Err, tr!("检查更新失败"), e),
                _ => {}
            }
        }
        let job = self.update_job.lock().unwrap().clone();
        if let UpdateJob::Ready(exe) = job {
            match crate::update::apply_and_restart(&exe) {
                Ok(()) => self.quit(ctx),
                Err(e) => *self.update_job.lock().unwrap() = UpdateJob::Failed(e),
            }
        }
    }

    fn start_update(&mut self) {
        let Some(rel) = self.update.clone() else { return };
        *self.update_job.lock().unwrap() = UpdateJob::Downloading;
        let (job, client, m) = (self.update_job.clone(), self.m.client(), self.m.clone());
        self.m.rt.spawn(async move {
            let dir = crate::engine::store::data_dir().join("update");
            let r = crate::update::download(&client, &rel, &dir).await;
            *job.lock().unwrap() = match r {
                Ok(p) => UpdateJob::Ready(p),
                Err(e) => UpdateJob::Failed(e),
            };
            m.repaint();
        });
    }

    fn update_banner(&mut self, ui: &mut Ui, p: &Pal) {
        let Some(rel) = self.update.clone() else { return };
        if self.m.settings.read().unwrap().skip_version == rel.version {
            return;
        }
        let job = self.update_job.lock().unwrap().clone();
        Frame::new()
            .fill(p.accent_soft)
            .stroke(Stroke::new(1.0, p.accent.gamma_multiply(0.5)))
            .corner_radius(10)
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                    icons::paint(ui.painter(), r, Icon::Download, p.accent);
                    ui.label(RichText::new(trf!("发现新版本 v{}", rel.version)).font(bold(14.0)).color(p.text));
                    ui.label(RichText::new(trf!("当前 v{}", env!("CARGO_PKG_VERSION"))).size(12.5).color(p.weak));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| match &job {
                        UpdateJob::Downloading | UpdateJob::Ready(_) => {
                            ui.label(RichText::new(tr!("正在下载并校验更新")).size(12.5).color(p.weak));
                            ui.add(egui::Spinner::new().size(14.0));
                        }
                        _ => {
                            if button(ui, Btn::Primary, Some(Icon::Download), tr!("立即更新")).clicked() {
                                self.start_update();
                            }
                            if button(ui, Btn::Secondary, None, tr!("查看更新内容")).clicked() {
                                let _ = std::process::Command::new("explorer").arg(&rel.page).spawn();
                            }
                            if button(ui, Btn::Secondary, None, tr!("忽略此版本")).clicked() {
                                let mut s = self.m.settings.read().unwrap().clone();
                                s.skip_version = rel.version.clone();
                                self.m.apply_settings(s);
                            }
                        }
                    });
                });
                if let UpdateJob::Failed(e) = &job {
                    ui.label(RichText::new(e).size(12.5).color(p.danger));
                }
            });
        ui.add_space(8.0);
    }

    /// 任务栏进度条与托盘提示
    fn update_system(&mut self, rows: &[Row]) {
        let running: Vec<&Row> = rows.iter().filter(|r| r.status == Status::Running).collect();
        let (done, total) = running.iter().fold((0f64, 0f64), |(d, t), r| match (r.segs, r.size) {
            (Some((sd, sn)), _) => (d + sd as f64 / sn.max(1) as f64, t + 1.0),
            (_, Some(s)) if s > 0 => (d + r.done as f64 / s as f64, t + 1.0),
            _ => (d, t),
        });
        let frac = if total > 0.0 { (done / total) as f32 } else { 0.0 };
        let queued = rows.iter().any(|r| r.status == Status::Queued);
        let failed = rows.iter().any(|r| r.status == Status::Error);
        let p = if !running.is_empty() && failed {
            // 仍在下载但有任务失败：红色提醒
            crate::winsys::Progress::Error(frac)
        } else if !running.is_empty() {
            crate::winsys::Progress::Normal(frac)
        } else if queued {
            crate::winsys::Progress::Paused(0.0)
        } else {
            crate::winsys::Progress::None
        };
        if let Some(tb) = &mut self.taskbar {
            tb.set(p);
        }
        if self.tray_tip.1.elapsed() >= Duration::from_secs(1)
            && let Some(tray) = &self.tray
        {
            self.tray_tip.1 = Instant::now();
            let speed: u64 = running.iter().map(|r| r.speed).sum();
            let tip = if running.is_empty() {
                tr!("SwiftDL 下载器  空闲").to_string()
            } else {
                trf!("SwiftDL 下载器\n下载中 {} 个  {}", running.len(), fmt_speed(speed))
            };
            if tip != self.tray_tip.0 {
                let _ = tray.icon.set_tooltip(Some(&tip));
                self.tray_tip.0 = tip;
            }
        }
    }

    /// 拖放悬停时的提示层
    fn drop_overlay(&self, ctx: &egui::Context) {
        if !self.drop_over.as_ref().is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed)) {
            return;
        }
        let p = theme::pal_ctx(ctx);
        let rect = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::unique("drop_overlay")));
        painter.rect_filled(rect, 0, p.bg.gamma_multiply(0.85));
        let card = Rect::from_center_size(rect.center(), vec2(380.0, 180.0));
        painter.rect(card, 16, p.panel, Stroke::new(2.0, p.accent), StrokeKind::Inside);
        icons::paint(&painter, Rect::from_center_size(card.center() - vec2(0.0, 28.0), vec2(40.0, 40.0)), Icon::Download, p.accent);
        painter.text(card.center() + vec2(0.0, 24.0), Align2::CENTER_CENTER, tr!("松开鼠标添加下载"), bold(17.0), p.text);
        painter.text(card.center() + vec2(0.0, 52.0), Align2::CENTER_CENTER, tr!("支持链接、文本、.txt 链接列表、.url 快捷方式"), prop(12.5), p.weak);
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn shortcuts(&mut self, ctx: &egui::Context, rows: &[Row]) {
        if self.add.is_some() || self.settings.is_some() || self.delete.is_some() || self.relink.is_some() || self.ext_guide.is_some() {
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
            self.sel = self.visible(rows).iter().map(|r| r.t.id).collect();
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
                .filter(|l| {
                    ["http://", "https://", "ftp://", "ftps://", "sftp://"].iter().any(|p| l.to_ascii_lowercase().starts_with(p)) || crate::engine::bt::is_bt(l)
                })
                .map(|u| AddReq { url: u.into(), ..Default::default() })
                .collect();
            if !reqs.is_empty() {
                self.open_add(ctx, reqs);
            }
        }
    }

    /// 全部完成后的动作
    fn run_after_all(&mut self, ctx: &egui::Context) {
        use std::os::windows::process::CommandExt;
        let action = std::mem::replace(&mut self.after_all, AfterAll::Nothing);
        self.countdown_at = None;
        match action {
            AfterAll::Nothing => {}
            AfterAll::Exit => self.quit(ctx),
            AfterAll::Sleep => {
                self.m.save();
                unsafe { windows_sys::Win32::System::Power::SetSuspendState(false, false, false) };
            }
            AfterAll::Shutdown => {
                self.m.save();
                let _ = std::process::Command::new("shutdown").args(["/s", "/t", "0"]).creation_flags(0x0800_0000).spawn();
                self.quit(ctx);
            }
        }
    }
}

impl eframe::App for App {
    /// 每帧（包括窗口隐藏在托盘时）都会调用：处理托盘、关闭、完成动作
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 状态变化、任务栏进度、托盘提示：窗口隐藏在托盘时也要更新
        let rows = self.snapshot();
        self.detect_changes(ctx, &rows);
        self.update_system(&rows);
        self.poll_update(ctx, false);
        if self.first_frame {
            self.first_frame = false;
            if self.start_hidden {
                self.hide(ctx);
            } else if !self.m.settings.read().unwrap().ext_guide_done && !crate::browser::connected() {
                self.ext_guide = Some(ExtGuide::new());
            }
        }
        // 扩展连接：首次连接后不再自动弹出安装向导
        if crate::browser::connected() && !self.ext_was_connected {
            self.ext_was_connected = true;
            let mut s = self.m.settings.read().unwrap().clone();
            if !s.ext_guide_done {
                s.ext_guide_done = true;
                self.m.apply_settings(s);
                self.toast(None, ToastKind::Ok, tr!("浏览器扩展已连接"), tr!("浏览器中的下载将自动交给 SwiftDL").into());
            }
        }
        let cmds: Vec<TrayCmd> = self.tray.as_ref().map(|t| t.rx.try_iter().collect()).unwrap_or_default();
        for c in cmds {
            match c {
                TrayCmd::Show => self.show(ctx),
                TrayCmd::StartAll => self.all_ids().into_iter().for_each(|id| self.m.start(id)),
                TrayCmd::PauseAll => self.all_ids().into_iter().for_each(|id| self.m.pause(id)),
                TrayCmd::Quit => self.quit(ctx),
            }
        }
        // 关闭按钮：最小化到托盘（首次询问）
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.tray.is_some() {
            let s = self.m.settings.read().unwrap().clone();
            if !s.tray_hint_shown {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.close_ask = Some(CloseAsk::new());
            } else if s.close_to_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hide(ctx);
            }
        }
        // 全部完成后的动作：从“有任务在跑”变为“全部空闲”时开始倒计时
        let busy = self.m.tasks.read().unwrap().iter().any(|t| matches!(t.status(), Status::Running | Status::Queued));
        if self.after_all != AfterAll::Nothing && self.was_busy && !busy && self.countdown_at.is_none() {
            self.countdown_at = Some(Instant::now());
            self.show(ctx);
        }
        if busy {
            self.countdown_at = None;
        }
        self.was_busy = busy;
        if let Some(t0) = self.countdown_at {
            if t0.elapsed().as_secs() >= COUNTDOWN {
                self.run_after_all(ctx);
            } else {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
    }

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
                UiEvent::Show => self.show(&ctx),
            }
        }
        let rows = self.snapshot();
        self.shortcuts(&ctx, &rows);
        self.sample_speed(&rows);
        let p = theme::pal(ui);

        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(212.0)
            .frame(Frame::new().fill(p.side).inner_margin(Margin { left: 14, right: 14, top: 16, bottom: 14 }))
            .show(ui, |ui| self.sidebar(ui, &rows));
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
            .show(ui, |ui| self.list(ui, &rows));

        if let Some(d) = &mut self.add
            && matches!(d.show(&ctx, &self.m), DialogResult::Close)
        {
            self.add = None;
        }
        if let Some(d) = &mut self.settings {
            match d.show(&ctx, &self.m) {
                DialogResult::Close => self.settings = None,
                DialogResult::OpenExtGuide => {
                    self.settings = None;
                    self.ext_guide = Some(ExtGuide::new());
                }
                DialogResult::CheckUpdate => self.poll_update(&ctx, true),
                DialogResult::None => {}
            }
        }
        if let Some(d) = &mut self.delete
            && matches!(d.show(&ctx, &self.m), DialogResult::Close)
        {
            for id in &d.ids {
                self.sel.remove(id);
            }
            self.delete = None;
        }
        if let Some(d) = &mut self.relink
            && matches!(d.show(&ctx, &self.m), DialogResult::Close)
        {
            self.relink = None;
        }
        if let Some(d) = &mut self.ext_guide
            && matches!(d.show(&ctx, &self.m), DialogResult::Close)
        {
            self.ext_guide = None;
        }
        if let Some(d) = &mut self.close_ask {
            let choice = d.show(&ctx);
            let remember_as = |tray: bool| {
                let mut s = self.m.settings.read().unwrap().clone();
                s.close_to_tray = tray;
                s.tray_hint_shown = true;
                self.m.apply_settings(s);
            };
            match choice {
                CloseChoice::None => {}
                CloseChoice::Cancel => self.close_ask = None,
                CloseChoice::Tray { remember } => {
                    if remember {
                        remember_as(true);
                    }
                    self.close_ask = None;
                    self.hide(&ctx);
                }
                CloseChoice::Quit { remember } => {
                    if remember {
                        remember_as(false);
                    }
                    self.close_ask = None;
                    self.quit(&ctx);
                }
            }
        }
        if let Some(t0) = self.countdown_at {
            let left = COUNTDOWN.saturating_sub(t0.elapsed().as_secs());
            match dialogs::countdown(&ctx, self.after_all.verb(), left) {
                Some(true) => self.run_after_all(&ctx),
                Some(false) => {
                    self.countdown_at = None;
                    self.after_all = AfterAll::Nothing;
                }
                None => {}
            }
        }
        self.toasts(&ctx);
        self.drop_overlay(&ctx);

        // 仅在有任务活动时定时刷新，空闲时零 CPU
        if rows.iter().any(|r| r.status == Status::Running) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

fn paint_row(ui: &Ui, p: &Pal, card: Rect, r: &Row, selected: bool, hovered: bool) {
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

    let st = r.status;
    let (size, done, speed) = (r.size, r.done, r.speed);
    let planned = st == Status::Paused && r.scheduled;
    let sc = if planned { p.warning } else { status_color(p, st) };

    let badge = Rect::from_min_size(pos2(card.min.x + 12.0, card.center().y - 19.0), vec2(38.0, 38.0));
    file_badge(ui, badge, &r.name);
    let x0 = badge.max.x + 14.0;
    let xr = card.max.x - 16.0;
    let y1 = card.min.y + 20.0;
    let y2 = card.min.y + 41.0;

    // 第一行：文件名 + 右侧速度/状态
    let right1 = if st == Status::Running {
        let txt = if speed > 0 { fmt_speed(speed) } else { tr!("连接中").into() };
        let g = painter.layout_no_wrap(txt, bold(13.5), p.text);
        let w = g.size().x;
        painter.galley(pos2(xr - w, y1 - g.size().y / 2.0), g, p.text);
        w
    } else {
        let g = painter.layout_no_wrap(status_text(st, r.scheduled).into(), prop(13.0), sc);
        let w = g.size().x;
        painter.galley(pos2(xr - w, y1 - g.size().y / 2.0), g, sc);
        let icon = if planned { Icon::Clock } else { status_icon(st) };
        icons::paint(painter, Rect::from_center_size(pos2(xr - w - 11.0, y1), vec2(14.0, 14.0)), icon, sc);
        w + 20.0
    };
    let name_clip = Rect::from_min_max(pos2(x0, card.min.y), pos2(xr - right1 - 16.0, card.max.y));
    let g = painter.layout_no_wrap(r.name.clone(), prop(14.5), p.text);
    let gh = g.size().y;
    painter.with_clip_rect(name_clip).galley(pos2(x0, y1 - gh / 2.0), g, p.text);

    // 第二行：进度条 + 百分比 + 大小 + 右侧剩余时间/完成时间/错误
    let hls = r.segs.filter(|_| st != Status::Done);
    let frac = match (st, size, hls) {
        (Status::Done, _, _) => 1.0,
        (_, _, Some((d, n))) => d as f32 / n as f32,
        (_, Some(s), _) if s > 0 => done as f32 / s as f32,
        _ => 0.0,
    };
    // HLS 总大小按已完成分片的平均大小估算
    let est_total = hls.filter(|(d, _)| *d > 0).map(|(d, n)| done / d as u64 * n as u64);
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
        _ if hls.is_some() => {
            let (d, n) = hls.unwrap_or_default();
            let est = est_total.map(|e| trf!("（约 {}）", fmt_size(e))).unwrap_or_default();
            trf!("第 {} / {} 段    {}{}", d, n, fmt_size(done), est)
        }
        (Status::Done, Some(s)) => fmt_size(s),
        (_, Some(s)) => format!("{:.1}%    {} / {}", frac * 100.0, fmt_size(done), fmt_size(s)),
        (_, None) if done > 0 => fmt_size(done),
        _ => String::new(),
    };
    painter.text(pos2(bar.max.x + 12.0, y2), Align2::LEFT_CENTER, info, prop(12.5), p.weak);
    let right2 = match st {
        Status::Running => size
            .or(est_total)
            .filter(|_| speed > 0)
            .map(|s| trf!("剩余 {}", fmt_eta(s.saturating_sub(done) / speed)))
            .unwrap_or_default(),
        Status::Done => fmt_ago(r.finished),
        Status::Error => r.error.clone().map(|e| e.chars().take(28).collect()).unwrap_or_default(),
        _ => String::new(),
    };
    let c2 = if st == Status::Error { p.danger } else { p.weak };
    painter.text(pos2(xr, y2), Align2::RIGHT_CENTER, right2, prop(12.5), c2);
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

fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

/// `select` 为 true 时在资源管理器中定位文件；否则用默认程序打开（无关联程序时让用户选择）
fn open_path(p: &std::path::Path, select: bool) {
    if select {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(p);
        let _ = std::process::Command::new("explorer").arg(arg).spawn();
    } else {
        crate::winsys::open_file(p, false);
    }
}

#[cfg(test)]
mod tests {
    /// 防回退：界面与引擎中显示给用户的字符串只允许 ASCII、汉字和常用中文标点，
    /// 杜绝在部分系统上会显示成方框的特殊符号与 emoji。
    #[test]
    fn ui_strings_only_safe_glyphs() {
        let ok = |c: char| c.is_ascii() || ('\u{4E00}'..='\u{9FFF}').contains(&c) || "，。：（）、！？；“”《》".contains(c);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = [
            "ui/mod.rs",
            "ui/dialogs.rs",
            "ui/widgets.rs",
            "ui/theme.rs",
            "ui/icons.rs",
            "engine/mod.rs",
            "engine/task.rs",
            "engine/probe.rs",
            "engine/kind.rs",
            "engine/hls.rs",
            "engine/remote.rs",
            "engine/bt.rs",
            "tray.rs",
            "browser.rs",
        ];
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
