//! 计划任务时间窗（本地时间，按天循环）。
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub enabled: bool,
    /// 开始时间 HH:MM
    pub start: String,
    /// 停止时间 HH:MM，空表示不自动停止
    pub stop: String,
}

impl Default for Schedule {
    fn default() -> Self {
        Self { enabled: false, start: "02:00".into(), stop: "07:00".into() }
    }
}

/// 解析 HH:MM 为当天分钟数
pub fn parse_hm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// 当前分钟是否处于计划时间窗内；无停止时间时，开始时间到当天结束都算窗口内。
/// 开始晚于停止表示跨午夜（如 23:00 - 06:00）。
pub fn in_window(now: u32, start: u32, stop: Option<u32>) -> bool {
    match stop {
        None => now >= start,
        Some(stop) if start <= stop => now >= start && now < stop,
        Some(stop) => now >= start || now < stop,
    }
}

impl Schedule {
    /// 返回 None 表示计划未启用或时间格式无效
    pub fn active(&self, now: u32) -> Option<bool> {
        if !self.enabled {
            return None;
        }
        let start = parse_hm(&self.start)?;
        Some(in_window(now, start, parse_hm(&self.stop)))
    }

    pub fn has_stop(&self) -> bool {
        parse_hm(&self.stop).is_some()
    }
}

/// 本地时间的当天分钟数
pub fn local_minutes() -> u32 {
    let mut t = unsafe { std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    t.wHour as u32 * 60 + t.wMinute as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows() {
        assert_eq!(parse_hm("02:30"), Some(150));
        assert_eq!(parse_hm(" 7:05 "), Some(425));
        assert_eq!(parse_hm("24:00"), None);
        assert_eq!(parse_hm(""), None);
        // 同日窗口
        assert!(in_window(150, 120, Some(420)));
        assert!(!in_window(420, 120, Some(420)));
        assert!(!in_window(100, 120, Some(420)));
        // 跨午夜
        assert!(in_window(23 * 60 + 30, 23 * 60, Some(6 * 60)));
        assert!(in_window(60, 23 * 60, Some(6 * 60)));
        assert!(!in_window(12 * 60, 23 * 60, Some(6 * 60)));
        // 无停止时间
        assert!(in_window(23 * 60, 120, None));
        assert!(!in_window(60, 120, None));
        let s = Schedule { enabled: true, start: "02:00".into(), stop: String::new() };
        assert_eq!(s.active(180), Some(true));
        assert!(!s.has_stop());
        assert_eq!(Schedule::default().active(180), None);
    }
}
