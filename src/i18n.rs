//! 多语言：以中文原文为键，英文表按需查找；`tr!` 取静态文本，`trf!` 填充 `{}` 占位符。
//! 英文译文可用 `{0}`、`{1}` 调整参数顺序。
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LangSetting {
    #[default]
    System,
    Zh,
    En,
}

static EN_ACTIVE: AtomicBool = AtomicBool::new(false);

fn system_is_chinese() -> bool {
    let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    id & 0x3ff == 0x04 // LANG_CHINESE
}

pub fn apply(s: LangSetting) {
    let en = match s {
        LangSetting::Zh => false,
        LangSetting::En => true,
        LangSetting::System => !system_is_chinese(),
    };
    EN_ACTIVE.store(en, Ordering::Relaxed);
}

pub fn is_en() -> bool {
    EN_ACTIVE.load(Ordering::Relaxed)
}

static EN: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| crate::i18n_en::EN.iter().copied().collect());

pub fn tr(zh: &'static str) -> &'static str {
    if is_en() { EN.get(zh).copied().unwrap_or(zh) } else { zh }
}

/// 填充占位符：`{}` 依次取参数，`{N}` 取第 N 个参数
pub fn fill(tpl: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::with_capacity(tpl.len() + 16);
    let mut next = 0;
    let mut rest = tpl;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('}') {
            Some(j) if after[..j].chars().all(|c| c.is_ascii_digit()) => {
                let idx = if j == 0 {
                    next += 1;
                    next - 1
                } else {
                    after[..j].parse().unwrap_or(usize::MAX)
                };
                if let Some(a) = args.get(idx) {
                    out.push_str(&a.to_string());
                }
                rest = &after[j + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

macro_rules! tr {
    ($s:literal) => {
        $crate::i18n::tr($s)
    };
}

macro_rules! trf {
    ($s:literal $(, $a:expr)* $(,)?) => {
        $crate::i18n::fill($crate::i18n::tr($s), &[$(&$a as &dyn std::fmt::Display),*])
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_placeholders() {
        assert_eq!(fill("第 {} / {} 段", &[&3, &10]), "第 3 / 10 段");
        assert_eq!(fill("{1} of {0}", &[&"a", &"b"]), "b of a");
        assert_eq!(fill("无占位", &[]), "无占位");
        assert_eq!(fill("{x} 保留", &[&1]), "{x} 保留");
    }

    /// 源码中每个 tr!/trf! 的中文键都必须有英文译文，占位符数量一致，且译文只含安全字形
    #[test]
    fn english_table_complete() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut keys = std::collections::BTreeSet::new();
        let mut stack = vec![root];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("i18n.rs") {
                    let src = std::fs::read_to_string(&p).unwrap();
                    for m in ["tr!(\"", "trf!(\""] {
                        let mut rest = src.as_str();
                        while let Some(i) = rest.find(m) {
                            rest = &rest[i + m.len()..];
                            let end = rest.find('"').unwrap();
                            keys.insert(rest[..end].replace("\\n", "\n"));
                            rest = &rest[end..];
                        }
                    }
                }
            }
        }
        assert!(keys.len() > 100, "应提取到大量界面文本，实际 {}", keys.len());
        let ok = |c: char| c.is_ascii() || "，。：（）、！？；“”《》".contains(c);
        let count = |s: &str| fill(s, &[&"\u{1}", &"\u{1}", &"\u{1}", &"\u{1}", &"\u{1}", &"\u{1}"]).matches('\u{1}').count();
        let mut bad = vec![];
        for k in &keys {
            match EN.get(k.as_str()) {
                None => bad.push(format!("缺少译文: {k:?}")),
                Some(v) => {
                    if !v.chars().all(ok) {
                        bad.push(format!("译文含非 ASCII: {v:?}"));
                    }
                    if count(k) != count(v) {
                        bad.push(format!("占位符数量不一致: {k:?} -> {v:?}"));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}
