//! 领域模型：常量、文件动作、复制记录、目标路径策略。
//!
//! 这里刻意不依赖 `ratatui`：界面用到的颜色映射放在 `tui` 层，
//! 引擎只产出语义（动作 / 说明 / 路径），这样引擎可以脱离终端被测试。

use std::path::{Path, PathBuf};

use crate::datefmt;

/// 版本号直接取 `Cargo.toml`，发版时只改一处。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 配置格式版本：v1 = 旧 `filecopy.py` 写出的 JSON（没有 `config_version`）。
pub const CONFIG_VERSION: u32 = 2;

/// 约定：0 成功 / 1 运行或复制失败 / 2 配置或用法错误 / 130 SIGINT / 143 SIGTERM。
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_SIGINT: i32 = 130;
pub const EXIT_SIGTERM: i32 = 143;

/// 并行线程上限。相机卡 / 单盘场景 64 已远超需求。
pub const MAX_WORKERS: usize = 64;

/// 默认复制缓冲区（与 `shutil.COPY_BUFSIZE` 一致，1 MiB）。
/// 0 或等于该值时走内核 `copy_file_range` / `sendfile` 快路径。
pub const DEFAULT_COPY_BUFSIZE: u64 = 1024 * 1024;

/// `--verify` 允许的算法，保持固定集合，避免不同平台可用算法不一致。
pub const VERIFY_ALGORITHMS: &[&str] = &["md5", "sha1", "sha256", "sha512", "blake2b"];

/// 日期格式候选：(显示文案, 格式)。
pub const DATE_FORMATS: &[(&str, &str)] = &[
    ("%Y/%m/%d — 2026/11/09", "%Y/%m/%d"),
    ("%Y/%m — 2026/11", "%Y/%m"),
    ("%Y-%m-%d — 2026-11-09", "%Y-%m-%d"),
    ("%Y-%m — 2026-11", "%Y-%m"),
    ("%Y/%m/%d/%H — 2026/11/09/19", "%Y/%m/%d/%H"),
];

/// ASCII 标志（与 Python 版一致，打印在 `--version` / `--help` 之前可选用）。
pub const ASCII_LOGO: &str = r#"
     ______ ____ __     ______ ______ ____   ____ __  __
    / ____//  _// /    / ____// ____// __ \ / __ \\ \/ /
   / /_    / / / /    / __/  / /    / / / // /_/ / \  /
  / __/  _/ / / /___ / /___ / /___ / /_/ // ____/  / /
 /_/    /___//_____//_____/ \____/ \____//_/      /_/
"#;

/// 单个文件的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileAction {
    Copied,
    Skipped,
    Overwritten,
    Failed,
}

impl FileAction {
    pub fn label(self) -> &'static str {
        match self {
            FileAction::Copied => "复制",
            FileAction::Skipped => "跳过",
            FileAction::Overwritten => "覆盖",
            FileAction::Failed => "失败",
        }
    }

    /// 是否真的写了盘。进度条分子只认这两种，与 `will_write` 的口径一致。
    pub fn is_written(self) -> bool {
        matches!(self, FileAction::Copied | FileAction::Overwritten)
    }
}

/// 一条文件级结果，界面把它塞进可滚动表格。
#[derive(Debug, Clone)]
pub struct CopyRecord {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub action: FileAction,
    pub detail: String,
}

/// 目标路径策略。
///
/// `date_tree` 是 Python 版旧 `date` 的行为（日期 + 源目录结构），
/// `tree` 是旧 `flat` 的行为（保留源目录结构）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Date,
    DateTree,
    Flat,
    Tree,
    Ext,
}

impl Strategy {
    pub const ALL: [Strategy; 5] = [
        Strategy::Date,
        Strategy::DateTree,
        Strategy::Flat,
        Strategy::Tree,
        Strategy::Ext,
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "date" => Some(Strategy::Date),
            "date_tree" => Some(Strategy::DateTree),
            "flat" => Some(Strategy::Flat),
            "tree" => Some(Strategy::Tree),
            "ext" => Some(Strategy::Ext),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Strategy::Date => "date",
            Strategy::DateTree => "date_tree",
            Strategy::Flat => "flat",
            Strategy::Tree => "tree",
            Strategy::Ext => "ext",
        }
    }

    /// 交互式选择用的短说明。
    pub fn ui_hint(self) -> &'static str {
        match self {
            Strategy::Date => "按日期，文件平铺",
            Strategy::DateTree => "按日期，保留结构",
            Strategy::Flat => "全平铺",
            Strategy::Tree => "保留目录结构",
            Strategy::Ext => "按扩展名",
        }
    }

    /// 交互式选择用的落点示例。
    ///
    /// `ext` 是界面上填的第一个扩展名，示例文件名跟着它走（`文件.jpg`），
    /// 不写死某个具体的相机/照片名，免得看着像只支持那一种文件。
    pub fn ui_example(self, ext: Option<&str>) -> String {
        let file = match ext {
            Some(ext) if !ext.is_empty() => format!("文件.{ext}"),
            _ => "文件".to_string(),
        };
        match self {
            Strategy::Date => format!("2026/11/08/{file}"),
            Strategy::DateTree => format!("2026/11/08/子目录/{file}"),
            Strategy::Flat => file,
            Strategy::Tree => format!("子目录/{file}"),
            Strategy::Ext => match ext {
                Some(ext) if !ext.is_empty() => format!("{ext}/{file}"),
                _ => "扩展名/文件".to_string(),
            },
        }
    }

    /// 实例级描述（date 系列要带上日期来源与格式）。
    pub fn describe(self, date_format: &str, date_source: DateSource) -> String {
        let when = date_source.ui_label();
        match self {
            Strategy::Date => format!("按{when}（{date_format}）分文件夹，文件平铺"),
            Strategy::DateTree => format!("按{when}（{date_format}）+ 保留源目录结构"),
            Strategy::Flat => "扁平结构 (不创建子文件夹)".to_string(),
            Strategy::Tree => "保留源目录结构".to_string(),
            Strategy::Ext => "按扩展名分文件夹".to_string(),
        }
    }

    pub fn keeps_tree(self) -> bool {
        matches!(self, Strategy::DateTree | Strategy::Tree)
    }
}

/// 策略计算目标路径时需要的外部上下文。
///
/// 用 `&mut` 携带警告收集器，避免策略层直接打印（引擎统一上报）。
pub struct PathCtx<'a> {
    pub src_root: &'a Path,
    pub dst_root: &'a Path,
    pub date_format: &'a str,
    pub date_source: DateSource,
    pub warnings: &'a mut Vec<String>,
}

/// 日期策略取哪个时间戳：文件创建时间（默认）还是最后修改时间。
///
/// 明确不用 EXIF：相机存储卡上的照片不一定带 EXIF，视频/RAW/其它文件更是没有，
/// 而创建/修改时间是所有文件都有的通用属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateSource {
    #[default]
    Created,
    Modified,
}

impl DateSource {
    pub const ALL: [DateSource; 2] = [DateSource::Created, DateSource::Modified];

    pub fn name(self) -> &'static str {
        match self {
            DateSource::Created => "created",
            DateSource::Modified => "modified",
        }
    }

    /// 界面上的中文名。
    pub fn ui_label(self) -> &'static str {
        match self {
            DateSource::Created => "创建日期",
            DateSource::Modified => "修改日期",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "created" | "creation" | "create" => Some(DateSource::Created),
            "modified" | "modify" | "mtime" => Some(DateSource::Modified),
            _ => None,
        }
    }
}

impl Strategy {
    pub fn target_path(self, src: &Path, ctx: &mut PathCtx<'_>) -> PathBuf {
        let name = src.file_name().map(|n| n.to_owned()).unwrap_or_default();
        match self {
            Strategy::Flat => ctx.dst_root.join(&name),
            Strategy::Ext => {
                let ext = src
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| "no_ext".to_string());
                ctx.dst_root.join(ext).join(&name)
            }
            Strategy::Tree => ctx.dst_root.join(relative_or_warn(
                src,
                ctx.src_root,
                ctx,
                "将直接放在目标根目录",
            )),
            Strategy::Date | Strategy::DateTree => {
                let date_part = datefmt::format_target_date(
                    src,
                    ctx.date_format,
                    ctx.date_source,
                    ctx.warnings,
                );
                if self == Strategy::DateTree {
                    let rel = relative_or_warn(src, ctx.src_root, ctx, "将直接放在日期文件夹下");
                    ctx.dst_root.join(&date_part).join(rel)
                } else {
                    ctx.dst_root.join(&date_part).join(&name)
                }
            }
        }
    }
}

/// 计算相对路径；算不出（不在 root 下）时回退到文件名并记一条警告。
fn relative_or_warn(src: &Path, root: &Path, ctx: &mut PathCtx<'_>, hint: &str) -> PathBuf {
    if let Ok(rel) = src.strip_prefix(root) {
        return rel.to_path_buf();
    }
    // resolve 后再试一次：软链或多层 `..` 拼出来的路径可能只有真实路径才可比
    if let (Ok(real_src), Ok(real_root)) = (src.canonicalize(), root.canonicalize()) {
        if let Ok(rel) = real_src.strip_prefix(&real_root) {
            return rel.to_path_buf();
        }
    }
    ctx.warnings
        .push(format!("警告: 无法计算相对路径: {}，{hint}", src.display()));
    PathBuf::from(src.file_name().map(|n| n.to_owned()).unwrap_or_default())
}

/// 1024 进制，与 Python 版 `format_size` 逐字一致；0 返回 `0 bytes`。
pub fn format_size(size: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if size == 0 {
        return "0 bytes".to_string();
    }
    let mut value = size as f64;
    for unit in UNITS {
        if value < 1024.0 {
            return format!("{value:.2} {unit}");
        }
        value /= 1024.0;
    }
    format!("{value:.2} {}", UNITS[UNITS.len() - 1])
}

/// 表格里显示用的路径：能相对化就相对化，否则用回退（默认原路径）。
pub fn display_path(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => path.display().to_string(),
    }
}

/// 目标路径去重键：大小写不敏感的文件系统上要折叠大小写。
///
/// `macOS` 的 APFS 默认大小写不敏感，`IMG_1.NEF` 与 `img_1.nef` 是同一个文件；
/// 直接用 Path 比较会把它们当成两个目标，自动改名也会静默失效。
pub fn norm_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_matches_python() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1), "1.00 B");
        assert_eq!(format_size(1023), "1023.00 B");
        assert_eq!(format_size(1024), "1.00 KiB");
        assert_eq!(format_size(1024 * 1024), "1.00 MiB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn strategy_names_roundtrip() {
        for s in Strategy::ALL {
            assert_eq!(Strategy::from_name(s.name()), Some(s));
        }
        assert_eq!(Strategy::from_name("nope"), None);
    }
}
