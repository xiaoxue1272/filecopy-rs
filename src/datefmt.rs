//! 日期格式：校验（拒绝未知格式符、路径穿越）与渲染（创建日期 → 目录名）。
//!
//! Python 版用 `strftime` + 白名单；Rust 版用 chrono，但保留同样的两条底线：
//! 1. 未知格式符必须报错，而不是像 macOS 的 strftime 那样原样吐出来；
//! 2. 渲染结果不能是绝对路径、不能含 `..` 段，否则会逃出目标目录。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Local};

use crate::model::DateSource;

/// strftime 认识的格式符（含平台常见扩展），与 Python 版白名单一致。
const KNOWN_STRFTIME: &str = "aAbBcdDeFgGhHIjklmMnpPrRsStTuUVwWxXyYzZ%+";

/// 进程内只提示一次「当前系统拿不到创建时间」。
static CREATION_WARNED: AtomicBool = AtomicBool::new(false);

/// 校验自定义日期格式，返回错误信息；合法返回 `None`。
pub fn validate_date_format(fmt: &str) -> Option<String> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' {
            if i + 1 >= chars.len() {
                return Some("无效的日期格式: 结尾的 % 不完整".to_string());
            }
            let spec = chars[i + 1];
            if !KNOWN_STRFTIME.contains(spec) {
                return Some(format!("无效的日期格式: 未知格式符 %{spec}"));
            }
            i += 2;
            continue;
        }
        i += 1;
    }

    // 白名单通过后再问 chrono 支不支持：chrono 不认识的（如 %+）宁可明确报错，
    // 也不能让路径里出现字面量 "%+"。
    if let Some(err) = chrono_unsupported(fmt) {
        return Some(err);
    }

    match try_format_now(fmt) {
        Ok(rendered) => {
            if !is_safe_date_part(&rendered) {
                return Some("日期格式不能生成绝对路径或包含 '..'（会逃出目标目录）".to_string());
            }
            None
        }
        Err(e) => Some(format!("无效的日期格式: {e}")),
    }
}

/// chrono 是否认识这段格式串里的每个格式符。
fn chrono_unsupported(fmt: &str) -> Option<String> {
    for item in StrftimeItems::new(fmt) {
        if matches!(item, Item::Error) {
            return Some(format!(
                "无效的日期格式: 本版本不支持该格式串 {fmt:?}（chrono 无法解析）"
            ));
        }
    }
    None
}

fn try_format_now(fmt: &str) -> Result<String, String> {
    let now: DateTime<Local> = Local::now();
    // chrono 的 Display 实现里，遇到无法渲染的 item 会写入空串而不是 panic；
    // 这里用 write 到 String 再拿结果，保持与 Python strftime 一致的「渲染一次」语义。
    use std::fmt::Write as _;
    let mut out = String::new();
    write!(out, "{}", now.format(fmt)).map_err(|e| e.to_string())?;
    Ok(out)
}

/// 渲染结果不能是绝对路径、不能带盘符、不能含 `..` 段。
pub fn is_safe_date_part(rendered: &str) -> bool {
    let normalized = rendered.replace('\\', "/");
    if normalized.starts_with('/') {
        return false;
    }
    // Windows 盘符：C:\... / C:/...
    let bytes: Vec<char> = normalized.chars().collect();
    if bytes.len() > 1 && bytes[1] == ':' {
        return false;
    }
    !normalized.split('/').any(|seg| seg == "..")
}

/// 给某个时间点做格式化（与 `now` 同样的渲染路径）。
///
/// chrono 的 `Display` 在遇到非法格式串时会返回 `Err`，而 `to_string()` 遇到
/// `Err` 会 panic（"a Display implementation returned an error unexpectedly"）。
/// 界面里的日期格式是用户随手输入的，随时可能是半截 `%`，所以这里自己接管错误：
/// 渲染不出来就返回空串，由调用方决定怎么兜底。
pub fn format_time(time: SystemTime, fmt: &str) -> String {
    use std::fmt::Write;
    let dt: DateTime<Local> = DateTime::from(time);
    let mut out = String::new();
    if write!(out, "{}", dt.format(fmt)).is_err() {
        return String::new();
    }
    out
}

/// 按用户选的时间来源取文件时间戳。
///
/// 选「创建日期」但系统给不出创建时间时回退到修改时间，并只警告一次；
/// 选「修改日期」就直接用修改时间，拿不到再退回创建时间/当前时间。
pub fn file_time(path: &Path, source: DateSource, warnings: &mut Vec<String>) -> SystemTime {
    match std::fs::metadata(path) {
        Ok(meta) => match source {
            DateSource::Modified => match meta.modified() {
                Ok(modified) => modified,
                Err(_) => meta.created().unwrap_or_else(|_| SystemTime::now()),
            },
            DateSource::Created => {
                if let Ok(created) = meta.created() {
                    return created;
                }
                if !CREATION_WARNED.swap(true, Ordering::Relaxed) {
                    warnings
                        .push("警告: 当前系统不支持获取创建时间，将使用修改时间替代".to_string());
                }
                meta.modified().unwrap_or_else(|_| SystemTime::now())
            }
        },
        Err(_) => SystemTime::now(),
    }
}

/// 兼容旧入口：按创建日期取时间。
pub fn creation_time(path: &Path, warnings: &mut Vec<String>) -> SystemTime {
    file_time(path, DateSource::Created, warnings)
}

/// 策略层用的入口：按选定的时间来源渲染出目录名。
pub fn format_target_date(
    src: &Path,
    fmt: &str,
    source: DateSource,
    warnings: &mut Vec<String>,
) -> String {
    let time = file_time(src, source, warnings);
    let rendered = format_time(time, fmt);
    if rendered.is_empty() {
        warnings.push(format!("警告: 日期格式 {fmt:?} 无法渲染，已退回当前日期"));
        return format_time(SystemTime::now(), "%Y/%m/%d");
    }
    if !is_safe_date_part(&rendered) {
        warnings.push(format!(
            "警告: 日期格式渲染出非法路径 {rendered:?}，已退回当前日期"
        ));
        return format_time(SystemTime::now(), fmt);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_common_formats() {
        for fmt in [
            "%Y/%m/%d",
            "%Y-%m",
            "%Y-%m-%d",
            "%Y/%m/%d/%H",
            "%Y%m%d_%H%M%S",
        ] {
            assert_eq!(validate_date_format(fmt), None, "fmt={fmt}");
        }
    }

    #[test]
    fn rejects_unknown_and_trailing_percent() {
        assert!(validate_date_format("%Q").is_some());
        assert!(validate_date_format("%Y/").is_none());
        assert!(validate_date_format("abc%").is_some());
    }

    #[test]
    fn rejects_traversal_results() {
        // 结果里带 `..` 或绝对路径的格式必须挡住
        assert!(!is_safe_date_part("/etc"));
        assert!(!is_safe_date_part("../x"));
        assert!(!is_safe_date_part("a/../../b"));
        assert!(is_safe_date_part("2026/11/08"));
        assert!(!is_safe_date_part("C:/Windows"));
    }

    #[test]
    fn format_time_renders() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let s = format_time(t, "%Y");
        assert!(s.len() == 4 && s.chars().all(|c| c.is_ascii_digit()));
    }
}
