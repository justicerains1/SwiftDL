//! 持久化：设置在 %APPDATA%\swiftdl\settings.json（先写临时文件再原子替换），
//! 任务在 SQLite 库 swiftdl.db（每个任务一行，只写变化的行）；数据库不可用时回退到 tasks.json。
use super::{AddReq, Settings, Status};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Serialize, Deserialize)]
pub struct TaskRec {
    pub id: u64,
    pub req: AddReq,
    pub name: String,
    pub dir: PathBuf,
    pub conns: u32,
    pub size: Option<u64>,
    pub resumable: bool,
    pub validator: Option<String>,
    pub probed: bool,
    pub status: Status,
    pub error: Option<String>,
    pub created: u64,
    pub finished: u64,
    pub downloaded: u64,
    /// 每段 [已写入位置, 结束位置)
    pub segs: Vec<(u64, u64)>,
    // v0.2 新增字段：旧数据缺省兼容
    #[serde(default)]
    pub auto_dir: bool,
    #[serde(default)]
    pub scheduled: bool,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub hls: Option<super::hls::HlsState>,
    #[serde(default)]
    pub bt: Option<super::bt::BtState>,
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into());
    base.join("swiftdl")
}

fn read<T: for<'a> Deserialize<'a>>(name: &str) -> Option<T> {
    let path = data_dir().join(name);
    let s = std::fs::read(&path).ok()?;
    match serde_json::from_slice(&s) {
        Ok(v) => Some(v),
        Err(e) => {
            // 文件损坏：调用方会回落到默认值，随后的保存将覆盖原文件，先留一份副本
            log_warn!("{name} is corrupt ({e}); keeping a copy as {name}.bad");
            let _ = std::fs::copy(&path, data_dir().join(format!("{name}.bad")));
            None
        }
    }
}

pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

static SAVE_FAILING: AtomicBool = AtomicBool::new(false);

/// 记录保存结果。保存每隔几秒就有一次，因此只在“开始失败”和“恢复正常”时各写一条日志
pub fn note_save(what: &str, err: Option<&dyn std::fmt::Display>) {
    match err {
        Some(e) if !SAVE_FAILING.swap(true, Ordering::Relaxed) => log_error!("saving {what} failed: {e}"),
        None if SAVE_FAILING.swap(false, Ordering::Relaxed) => log_info!("saving {what} works again"),
        _ => {}
    }
}

/// 先写临时文件再原子替换；返回是否成功
pub fn write_bytes(name: &str, bytes: &[u8]) -> bool {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let tmp = dir.join(format!("{name}.tmp"));
    let r = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, dir.join(name)));
    note_save(name, r.as_ref().err().map(|e| e as &dyn std::fmt::Display));
    r.is_ok()
}

pub fn load_settings() -> Settings {
    read("settings.json").unwrap_or_default()
}
pub fn save_settings(s: &Settings) {
    write_bytes("settings.json", &encode(s));
}
/// 读取任务列表（优先 SQLite；数据库不可用时回退到 JSON）
pub fn load_tasks() -> Vec<TaskRec> {
    match Db::open() {
        Some(mut db) => db.load(),
        None => read("tasks.json").unwrap_or_default(),
    }
}

// ───────────── SQLite 任务库 ─────────────

/// 任务库：每个任务一行，只写入有变化的行（替代整表 JSON 重写）
pub struct Db {
    conn: rusqlite::Connection,
    /// 已写入的 (队列位置, JSON) 缓存，用于判断哪些行需要更新
    cache: std::collections::HashMap<u64, (i64, Vec<u8>)>,
}

impl Db {
    pub fn open() -> Option<Db> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir).ok()?;
        let conn = rusqlite::Connection::open(dir.join("swiftdl.db")).ok()?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS tasks (id INTEGER PRIMARY KEY, pos INTEGER NOT NULL, json BLOB NOT NULL);",
        )
        .ok()?;
        Some(Db { conn, cache: Default::default() })
    }

    /// 读取全部任务（按队列顺序）；库为空且存在旧版 tasks.json 时自动迁移并备份
    pub fn load(&mut self) -> Vec<TaskRec> {
        let rows: Vec<(u64, i64, Vec<u8>)> = self
            .conn
            .prepare("SELECT id, pos, json FROM tasks ORDER BY pos")
            .and_then(|mut st| st.query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?)))?.collect())
            .unwrap_or_default();
        if rows.is_empty() {
            let legacy = data_dir().join("tasks.json");
            if let Some(recs) = read::<Vec<TaskRec>>("tasks.json")
                && !recs.is_empty()
            {
                // 写入数据库成功后才把旧文件改名备份；失败时保留原文件，下次启动再迁移
                match self.save(&recs) {
                    Ok(_) => {
                        let _ = std::fs::rename(&legacy, data_dir().join("tasks.json.bak"));
                    }
                    Err(e) => log_error!("migrating tasks.json to the database failed: {e}"),
                }
                return recs;
            }
        }
        let mut out = Vec::with_capacity(rows.len());
        for (id, pos, json) in rows {
            match serde_json::from_slice::<TaskRec>(&json) {
                Ok(r) => {
                    self.cache.insert(id, (pos, json));
                    out.push(r);
                }
                Err(e) => log_warn!("task {id} in the database cannot be read and is skipped: {e}"),
            }
        }
        out
    }

    /// 保存：只写入新增/变化/删除的行，返回写入的行数。
    /// 任何一行失败都回滚整个事务且不更新缓存，下次保存会重试这些行
    pub fn save(&mut self, recs: &[TaskRec]) -> rusqlite::Result<usize> {
        let mut changed: Vec<(u64, i64, Vec<u8>)> = Vec::new();
        for (pos, r) in recs.iter().enumerate() {
            let json = encode(r);
            let pos = pos as i64;
            if self.cache.get(&r.id).is_none_or(|(p, j)| *p != pos || *j != json) {
                changed.push((r.id, pos, json));
            }
        }
        let alive: std::collections::HashSet<u64> = recs.iter().map(|r| r.id).collect();
        let removed: Vec<u64> = self.cache.keys().filter(|id| !alive.contains(id)).copied().collect();
        if changed.is_empty() && removed.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        {
            let mut up = tx.prepare_cached(
                "INSERT INTO tasks (id, pos, json) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET pos = ?2, json = ?3",
            )?;
            for (id, pos, json) in &changed {
                up.execute(rusqlite::params![*id as i64, pos, json])?;
            }
            for id in &removed {
                tx.execute("DELETE FROM tasks WHERE id = ?1", [*id as i64])?;
            }
        }
        tx.commit()?;
        let n = changed.len() + removed.len();
        for (id, pos, json) in changed {
            self.cache.insert(id, (pos, json));
        }
        for id in &removed {
            self.cache.remove(id);
        }
        Ok(n)
    }
}
