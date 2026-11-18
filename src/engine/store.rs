//! 持久化：%APPDATA%\swiftdl\{settings,tasks}.json，先写临时文件再原子替换。
use super::{AddReq, Settings, Status};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into());
    base.join("swiftdl")
}

fn read<T: for<'a> Deserialize<'a>>(name: &str) -> Option<T> {
    let s = std::fs::read(data_dir().join(name)).ok()?;
    serde_json::from_slice(&s).ok()
}

pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

pub fn write_bytes(name: &str, bytes: &[u8]) {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let tmp = dir.join(format!("{name}.tmp"));
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(name));
    }
}

pub fn load_settings() -> Settings {
    read("settings.json").unwrap_or_default()
}
pub fn save_settings(s: &Settings) {
    write_bytes("settings.json", &encode(s))
}
pub fn load_tasks() -> Vec<TaskRec> {
    read("tasks.json").unwrap_or_default()
}
