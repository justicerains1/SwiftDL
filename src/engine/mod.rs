//! 下载管理器：任务表、队列调度、限速、持久化、定期清理缓存、计划任务。
pub mod kind;
pub mod limiter;
pub mod probe;
pub mod sched;
pub mod store;
pub mod task;
#[cfg(test)]
mod tests;

use limiter::Limiter;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

pub const PART_EXT: &str = "swdl";
/// 出错任务自动重试间隔（秒）
const RETRY_DELAY: u64 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Status {
    Queued,
    Running,
    Paused,
    Done,
    Error,
}

impl Status {
    const ALL: [Status; 5] = [Self::Queued, Self::Running, Self::Paused, Self::Done, Self::Error];
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "排队中",
            Self::Running => "下载中",
            Self::Paused => "已暂停",
            Self::Done => "已完成",
            Self::Error => "出错",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AddReq {
    pub url: String,
    pub filename: Option<String>,
    pub referrer: Option<String>,
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub dir: PathBuf,
    pub conns: u32,
    pub max_active: u32,
    pub speed_limit_kb: u64,
    pub proxy: String,
    pub clipboard: bool,
    pub exts: String,
    pub port: u16,
    /// 已完成记录保留天数，0 = 永久保留
    pub clean_done_days: u32,
    pub theme: ThemeMode,
    pub close_to_tray: bool,
    pub autostart: bool,
    pub tray_hint_shown: bool,
    /// 出错任务自动重试次数，0 = 不重试
    pub auto_retry: u32,
    /// 按文件类型存入默认目录下的子目录
    pub categorize: bool,
    pub schedule: sched::Schedule,
    /// 浏览器扩展安装引导已完成或不再提示
    pub ext_guide_done: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let dir = std::env::var_os("USERPROFILE")
            .map(|p| PathBuf::from(p).join("Downloads"))
            .unwrap_or_else(|| "D:\\Downloads".into());
        Self {
            dir,
            conns: 8,
            max_active: 3,
            speed_limit_kb: 0,
            proxy: String::new(),
            clipboard: true,
            exts: "zip rar 7z gz xz bz2 tar iso img exe msi msix apk dmg deb rpm \
                   mp4 mkv avi mov wmv flv webm mp3 flac wav m4a aac pdf epub \
                   doc docx xls xlsx ppt pptx bin torrent"
                .into(),
            port: 52080,
            clean_done_days: 30,
            theme: ThemeMode::System,
            close_to_tray: true,
            autostart: false,
            tray_hint_shown: false,
            auto_retry: 3,
            categorize: true,
            schedule: sched::Schedule::default(),
            ext_guide_done: false,
        }
    }
}

pub struct Seg {
    pub st: Mutex<SegPos>,
    pub written: AtomicU64,
    pub busy: AtomicBool,
    /// 该分段累计失败的 worker 轮数
    pub fail_rounds: AtomicU32,
}

#[derive(Clone, Copy)]
pub struct SegPos {
    pub pos: u64,
    pub end: u64,
}

impl Seg {
    pub fn new(pos: u64, end: u64) -> Arc<Self> {
        Arc::new(Self {
            st: Mutex::new(SegPos { pos, end }),
            written: AtomicU64::new(pos),
            busy: AtomicBool::new(false),
            fail_rounds: AtomicU32::new(0),
        })
    }
}

pub struct Meta {
    pub name: String,
    pub dir: PathBuf,
    pub size: Option<u64>,
    pub resumable: bool,
    pub validator: Option<String>,
    pub probed: bool,
    pub error: Option<String>,
    pub created: u64,
    pub finished: u64,
    /// 使用默认目录添加，可按类型自动分类
    pub auto_dir: bool,
    /// 由计划任务启动
    pub scheduled: bool,
    /// 期望的 SHA-256（用户填写）
    pub sha256: Option<String>,
    /// 实际计算出的 SHA-256
    pub hash: Option<String>,
    pub hashing: bool,
    pub retries: u32,
    pub error_at: u64,
    /// 更新链接后首次续传：忽略 ETag 差异
    pub relink: bool,
}

pub struct Task {
    pub id: u64,
    req: RwLock<AddReq>,
    pub conns: u32,
    pub meta: Mutex<Meta>,
    status: AtomicU8,
    pub downloaded: AtomicU64,
    pub segs: Mutex<Vec<Arc<Seg>>>,
    pub speed: AtomicU64,
    last_bytes: AtomicU64,
    pub active_conns: AtomicU32,
    /// 自适应连接上限（遇到限流时下调）
    pub conn_limit: AtomicU32,
    /// 最近一次被限流的时间（秒）
    pub throttled_at: AtomicU64,
    pub cancel: Mutex<CancellationToken>,
}

impl Task {
    pub fn status(&self) -> Status {
        Status::ALL[self.status.load(Acquire) as usize]
    }
    pub fn set_status(&self, s: Status) {
        self.status.store(s as u8, Release);
    }
    pub fn req(&self) -> AddReq {
        self.req.read().unwrap().clone()
    }
    pub fn url(&self) -> String {
        self.req.read().unwrap().url.clone()
    }
    pub fn part_path(&self) -> PathBuf {
        let m = self.meta.lock().unwrap();
        m.dir.join(format!("{}.{PART_EXT}", m.name))
    }
    pub fn final_path(&self) -> PathBuf {
        let m = self.meta.lock().unwrap();
        m.dir.join(&m.name)
    }
    pub fn is_scheduled(&self) -> bool {
        self.meta.lock().unwrap().scheduled
    }

    fn to_rec(&self) -> store::TaskRec {
        let m = self.meta.lock().unwrap();
        let segs = self
            .segs
            .lock()
            .unwrap()
            .iter()
            .map(|s| (s.written.load(Acquire), s.st.lock().unwrap().end))
            .collect();
        let status = match self.status() {
            Status::Running => Status::Queued, // 重启后自动续传
            s => s,
        };
        store::TaskRec {
            id: self.id,
            req: self.req(),
            name: m.name.clone(),
            dir: m.dir.clone(),
            conns: self.conns,
            size: m.size,
            resumable: m.resumable,
            validator: m.validator.clone(),
            probed: m.probed,
            status,
            error: m.error.clone(),
            created: m.created,
            finished: m.finished,
            downloaded: self.downloaded.load(Relaxed),
            segs,
            auto_dir: m.auto_dir,
            scheduled: m.scheduled,
            sha256: m.sha256.clone(),
            hash: m.hash.clone(),
            retries: m.retries,
        }
    }

    fn from_rec(r: store::TaskRec) -> Arc<Self> {
        let segs: Vec<Arc<Seg>> = r.segs.iter().map(|&(w, e)| Seg::new(w, e)).collect();
        let downloaded = match (r.status, r.size) {
            (Status::Done, _) => r.size.unwrap_or(r.downloaded),
            (_, Some(sz)) if r.resumable => {
                sz - segs.iter().map(|s| s.st.lock().unwrap().end.saturating_sub(s.written.load(Relaxed))).sum::<u64>().min(sz)
            }
            _ => 0,
        };
        let conns = r.conns.clamp(1, 32);
        Arc::new(Task {
            id: r.id,
            req: RwLock::new(r.req),
            conns,
            meta: Mutex::new(Meta {
                name: r.name,
                dir: r.dir,
                size: r.size,
                resumable: r.resumable,
                validator: r.validator,
                probed: r.probed,
                error: r.error,
                created: r.created,
                finished: r.finished,
                auto_dir: r.auto_dir,
                scheduled: r.scheduled,
                sha256: r.sha256,
                hash: r.hash,
                hashing: false,
                retries: r.retries,
                error_at: now(),
                relink: false,
            }),
            status: AtomicU8::new(r.status as u8),
            downloaded: AtomicU64::new(downloaded),
            segs: Mutex::new(segs),
            speed: AtomicU64::new(0),
            last_bytes: AtomicU64::new(downloaded),
            active_conns: AtomicU32::new(0),
            conn_limit: AtomicU32::new(conns),
            throttled_at: AtomicU64::new(0),
            cancel: Mutex::new(CancellationToken::new()),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartMode {
    Now,
    Later,
    Scheduled,
}

/// 新建任务参数
pub struct NewTask {
    pub req: AddReq,
    pub name: String,
    pub dir: PathBuf,
    pub auto_dir: bool,
    pub conns: u32,
    pub mode: StartMode,
    pub sha256: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Top,
    Up,
    Down,
}

pub enum UiEvent {
    Add(Vec<AddReq>),
    Show,
}

pub struct Manager {
    pub tasks: RwLock<Vec<Arc<Task>>>,
    pub settings: RwLock<Settings>,
    pub limiter: Limiter,
    client: RwLock<reqwest::Client>,
    next_id: AtomicU64,
    dirty: AtomicBool,
    last_saved: Mutex<Vec<u8>>,
    sched_active: Mutex<Option<bool>>,
    pub rt: tokio::runtime::Handle,
    pub ui_ctx: OnceLock<eframe::egui::Context>,
    /// 程序自身写入剪贴板的内容，监听时忽略
    pub clip_ignore: Mutex<String>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn build_client(proxy: &str) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        // 多条独立 TCP 连接才能叠加带宽，故强制 HTTP/1.1
        .http1_only()
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(32);
    let p = proxy.trim();
    if p.eq_ignore_ascii_case("direct") {
        b = b.no_proxy();
    } else if !p.is_empty()
        && let Ok(px) = reqwest::Proxy::all(p)
    {
        b = b.proxy(px);
    }
    b.build().unwrap_or_default()
}

/// 计算文件 SHA-256（小写十六进制）
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

impl Manager {
    pub fn new(rt: tokio::runtime::Handle, mut settings: Settings) -> Arc<Self> {
        settings.max_active = settings.max_active.clamp(1, 10);
        settings.conns = settings.conns.clamp(1, 32);
        let tasks: Vec<Arc<Task>> = store::load_tasks().into_iter().map(Task::from_rec).collect();
        let next = tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        Arc::new(Self {
            tasks: RwLock::new(tasks),
            limiter: Limiter::new(settings.speed_limit_kb * 1024),
            client: RwLock::new(build_client(&settings.proxy)),
            settings: RwLock::new(settings),
            next_id: AtomicU64::new(next),
            dirty: AtomicBool::new(false),
            last_saved: Mutex::new(Vec::new()),
            sched_active: Mutex::new(None),
            rt,
            ui_ctx: OnceLock::new(),
            clip_ignore: Mutex::new(String::new()),
        })
    }

    pub fn client(&self) -> reqwest::Client {
        self.client.read().unwrap().clone()
    }

    pub fn repaint(&self) {
        if let Some(c) = self.ui_ctx.get() {
            c.request_repaint();
        }
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Release);
    }

    pub fn get(&self, id: u64) -> Option<Arc<Task>> {
        self.tasks.read().unwrap().iter().find(|t| t.id == id).cloned()
    }

    /// 链接是否已在列表中，返回该任务状态
    pub fn find_url(&self, url: &str) -> Option<Status> {
        self.tasks.read().unwrap().iter().find(|t| t.req.read().unwrap().url == url).map(|t| t.status())
    }

    pub fn apply_settings(self: &Arc<Self>, s: Settings) {
        let old = self.settings.read().unwrap().clone();
        if old.proxy != s.proxy {
            *self.client.write().unwrap() = build_client(&s.proxy);
        }
        if old.speed_limit_kb != s.speed_limit_kb {
            self.limiter.set_rate(s.speed_limit_kb * 1024);
        }
        if old.schedule != s.schedule {
            *self.sched_active.lock().unwrap() = None;
        }
        store::save_settings(&s);
        *self.settings.write().unwrap() = s;
        self.schedule();
    }

    pub fn add(self: &Arc<Self>, n: NewTask) -> u64 {
        let id = self.next_id.fetch_add(1, Relaxed);
        let name = if n.name.trim().is_empty() { String::new() } else { probe::sanitize(&n.name) };
        let sha256 = n.sha256.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
        let t = Task::from_rec(store::TaskRec {
            id,
            req: n.req,
            name,
            dir: n.dir,
            conns: n.conns,
            size: None,
            resumable: false,
            validator: None,
            probed: false,
            status: if n.mode == StartMode::Now { Status::Queued } else { Status::Paused },
            error: None,
            created: now(),
            finished: 0,
            downloaded: 0,
            segs: vec![],
            auto_dir: n.auto_dir,
            scheduled: n.mode == StartMode::Scheduled,
            sha256,
            hash: None,
            retries: 0,
        });
        self.tasks.write().unwrap().push(t);
        // 计划任务若正处于时间窗内，立即开始
        if n.mode == StartMode::Scheduled && *self.sched_active.lock().unwrap() == Some(true) {
            self.start(id);
        }
        self.mark_dirty();
        self.schedule();
        self.repaint();
        id
    }

    pub fn start(self: &Arc<Self>, id: u64) {
        if let Some(t) = self.get(id)
            && matches!(t.status(), Status::Paused | Status::Error)
        {
            t.meta.lock().unwrap().retries = 0;
            t.set_status(Status::Queued);
            self.mark_dirty();
        }
        self.schedule();
    }

    pub fn pause(&self, id: u64) {
        if let Some(t) = self.get(id) {
            match t.status() {
                Status::Running => t.cancel.lock().unwrap().cancel(),
                Status::Queued => t.set_status(Status::Paused),
                _ => {}
            }
            self.mark_dirty();
        }
    }

    /// 重新下载：清空进度
    pub fn restart(self: &Arc<Self>, id: u64) {
        let Some(t) = self.get(id) else { return };
        if t.status() == Status::Running {
            return;
        }
        let part = t.part_path();
        let _ = std::fs::remove_file(part);
        t.segs.lock().unwrap().clear();
        t.downloaded.store(0, Relaxed);
        t.last_bytes.store(0, Relaxed);
        {
            let mut m = t.meta.lock().unwrap();
            m.probed = false;
            m.hash = None;
            m.retries = 0;
        }
        t.set_status(Status::Queued);
        self.mark_dirty();
        self.schedule();
    }

    pub fn remove(self: &Arc<Self>, ids: &[u64], delete_file: bool) {
        let removed: Vec<Arc<Task>> = {
            let mut tasks = self.tasks.write().unwrap();
            let (rm, keep) = std::mem::take(&mut *tasks).into_iter().partition(|t| ids.contains(&t.id));
            *tasks = keep;
            rm
        };
        for t in &removed {
            t.cancel.lock().unwrap().cancel();
        }
        self.mark_dirty();
        self.schedule();
        // 等待下载线程释放文件句柄后再删除
        self.rt.spawn(async move {
            for t in removed {
                for _ in 0..100 {
                    if t.status() != Status::Running {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let _ = std::fs::remove_file(t.part_path());
                if delete_file && t.status() == Status::Done {
                    let _ = std::fs::remove_file(t.final_path());
                }
            }
        });
    }

    /// 更新失效链接：新链接大小一致才接受，然后继续续传
    pub async fn relink(self: &Arc<Self>, id: u64, url: String) -> Result<(), String> {
        let t = self.get(id).ok_or("任务不存在")?;
        if t.status() == Status::Running {
            self.pause(id);
            for _ in 0..100 {
                if t.status() != Status::Running {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let mut req = t.req();
        req.url = url.trim().to_string();
        let info = probe::probe(&self.client(), &req).await?;
        {
            let mut m = t.meta.lock().unwrap();
            if m.probed && m.size.is_some() && m.size != info.size {
                return Err(format!(
                    "新链接文件大小不一致（原 {} 字节，新 {} 字节）",
                    m.size.unwrap_or(0),
                    info.size.map(|s| s.to_string()).unwrap_or_else(|| "未知".into())
                ));
            }
            m.relink = true;
            m.error = None;
        }
        *t.req.write().unwrap() = req;
        if t.status() != Status::Done {
            self.start(id);
        }
        self.mark_dirty();
        self.repaint();
        Ok(())
    }

    /// 调整队列顺序（调度按列表顺序拉起任务）
    pub fn move_task(self: &Arc<Self>, id: u64, mv: Move) {
        {
            let mut tasks = self.tasks.write().unwrap();
            let Some(i) = tasks.iter().position(|t| t.id == id) else { return };
            match mv {
                Move::Top => {
                    let t = tasks.remove(i);
                    tasks.insert(0, t);
                }
                Move::Up if i > 0 => tasks.swap(i, i - 1),
                Move::Down if i + 1 < tasks.len() => tasks.swap(i, i + 1),
                _ => return,
            }
        }
        self.mark_dirty();
        self.repaint();
    }

    /// 后台计算已完成文件的 SHA-256
    pub fn hash_task(self: &Arc<Self>, id: u64) {
        let Some(t) = self.get(id) else { return };
        if t.status() != Status::Done {
            return;
        }
        {
            let mut m = t.meta.lock().unwrap();
            if m.hashing {
                return;
            }
            m.hashing = true;
        }
        let me = self.clone();
        self.rt.spawn_blocking(move || {
            let r = sha256_file(&t.final_path());
            let mut m = t.meta.lock().unwrap();
            m.hashing = false;
            m.hash = Some(r.unwrap_or_else(|e| format!("计算失败：{e}")));
            drop(m);
            me.mark_dirty();
            me.repaint();
        });
    }

    /// 按最大同时任务数拉起排队任务
    pub fn schedule(self: &Arc<Self>) {
        let max = self.settings.read().unwrap().max_active as usize;
        let tasks = self.tasks.read().unwrap();
        let mut running = tasks.iter().filter(|t| t.status() == Status::Running).count();
        for t in tasks.iter() {
            if running >= max {
                break;
            }
            if t.status() == Status::Queued {
                t.set_status(Status::Running);
                *t.cancel.lock().unwrap() = CancellationToken::new();
                running += 1;
                self.rt.spawn(task::run(self.clone(), t.clone()));
            }
        }
    }

    /// 给新任务分配不冲突的文件名
    pub fn reserve_name(&self, id: u64, dir: &Path, name: &str) -> String {
        let taken = |n: &str| {
            dir.join(n).exists()
                || dir.join(format!("{n}.{PART_EXT}")).exists()
                || self.tasks.read().unwrap().iter().any(|t| {
                    t.id != id && t.status() != Status::Done && {
                        let m = t.meta.lock().unwrap();
                        m.dir == dir && m.name == n
                    }
                })
        };
        unique_name(name, taken)
    }

    /// 每 500ms 调用：测速、出错自动重试、计划任务
    pub fn tick(self: &Arc<Self>) -> bool {
        let (auto_retry, schedule) = {
            let s = self.settings.read().unwrap();
            (s.auto_retry, s.schedule.clone())
        };
        let mut any = false;
        let mut requeue = false;
        let ts = now();
        for t in self.tasks.read().unwrap().iter() {
            let cur = t.downloaded.load(Relaxed);
            let last = t.last_bytes.swap(cur, Relaxed);
            match t.status() {
                Status::Running => {
                    any = true;
                    let inst = cur.saturating_sub(last) * 2;
                    let old = t.speed.load(Relaxed);
                    t.speed.store(if old == 0 { inst } else { (old * 6 + inst * 4) / 10 }, Relaxed);
                    continue;
                }
                Status::Error if auto_retry > 0 => {
                    let mut m = t.meta.lock().unwrap();
                    if m.retries < auto_retry && ts.saturating_sub(m.error_at) >= RETRY_DELAY {
                        m.retries += 1;
                        t.set_status(Status::Queued);
                        requeue = true;
                    }
                }
                _ => {}
            }
            t.speed.store(0, Relaxed);
        }
        // 计划任务：仅在时间窗边沿触发，避免与用户手动操作冲突
        let active = schedule.active(sched::local_minutes());
        let prev = std::mem::replace(&mut *self.sched_active.lock().unwrap(), active);
        if prev.is_some() && active != prev {
            let tasks: Vec<Arc<Task>> = self.tasks.read().unwrap().iter().filter(|t| t.is_scheduled()).cloned().collect();
            for t in tasks {
                match active {
                    Some(true) if t.status() == Status::Paused => {
                        t.set_status(Status::Queued);
                        requeue = true;
                    }
                    Some(false) if schedule.has_stop() => self.pause(t.id),
                    _ => {}
                }
            }
        }
        if requeue {
            self.mark_dirty();
            self.schedule();
            self.repaint();
        }
        any
    }

    /// 持久化：内容未变化时跳过写盘
    pub fn save(&self) {
        let recs: Vec<_> = self.tasks.read().unwrap().iter().map(|t| t.to_rec()).collect();
        self.dirty.store(false, Release);
        let bytes = store::encode(&recs);
        let mut last = self.last_saved.lock().unwrap();
        if *last == bytes {
            return;
        }
        store::write_bytes("tasks.json", &bytes);
        *last = bytes;
    }

    pub fn save_if_needed(&self, running: bool) {
        if running || self.dirty.load(Acquire) {
            self.save();
        }
    }

    /// 定期清缓存：过期完成记录、孤立的 .swdl 临时文件、残留的 .tmp 配置文件
    pub fn cleanup(&self) {
        let s = self.settings.read().unwrap().clone();
        if s.clean_done_days > 0 {
            let cutoff = now().saturating_sub(s.clean_done_days as u64 * 86400);
            let mut tasks = self.tasks.write().unwrap();
            let before = tasks.len();
            tasks.retain(|t| !(t.status() == Status::Done && t.meta.lock().unwrap().finished < cutoff));
            if tasks.len() != before {
                self.mark_dirty();
            }
        }
        let (dirs, parts) = {
            let tasks = self.tasks.read().unwrap();
            let mut dirs: Vec<PathBuf> = tasks.iter().map(|t| t.meta.lock().unwrap().dir.clone()).collect();
            dirs.push(s.dir.clone());
            dirs.sort();
            dirs.dedup();
            let parts: Vec<PathBuf> = tasks.iter().filter(|t| t.status() != Status::Done).map(|t| t.part_path()).collect();
            (dirs, parts)
        };
        let old = SystemTime::now() - Duration::from_secs(600);
        for d in dirs {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == PART_EXT)
                    && !parts.contains(&p)
                    && e.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < old)
                {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(store::data_dir()) {
            for e in rd.flatten() {
                if e.path().extension().is_some_and(|x| x == "tmp") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        self.repaint();
    }
}

pub fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|i| format!("{stem}({i}){ext}"))
        .find(|n| !taken(n))
        .unwrap()
}
