//! 配置：与 Python 版 JSON 完全兼容的读写、schema 严格校验、旧配置迁移。
//!
//! 两条不可退让的规则：
//! 1. `~` 与环境变量必须展开。不展开时 `~/Pictures` 会被当成相对路径，
//!    在 CWD 下建出一个名叫 `~` 的目录，照片归档到错误位置且毫无提示。
//! 2. 旧配置（没有 `config_version`、字段集合只有 7 个）里的 `date` / `flat`
//!    语义和现在不同，必须映射成 `date_tree` / `tree`，否则用户几百 GB 的
//!    照片落点会突然变成一锅平铺，还可能撞名丢文件。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::datefmt::validate_date_format;
pub use crate::model::DEFAULT_COPY_BUFSIZE;
use crate::model::{Strategy, CONFIG_VERSION, MAX_WORKERS, VERIFY_ALGORITHMS};

/// 旧 `filecopy.py` 的 `Config.to_dict` 写死这 7 个 key，一个不多一个不少。
const LEGACY_CONFIG_KEYS: [&str; 7] = [
    "source_dir",
    "target_dir",
    "extensions",
    "overwrite",
    "strategy",
    "date_format",
    "buffer_size",
];

/// 内存与磁盘表示分离：字段名对齐 JSON key，`strategy` 用字符串以便原样回写。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub source_dir: String,
    pub target_dir: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default = "default_date_format")]
    pub date_format: String,
    /// 日期策略用哪个时间戳：`created`（默认）或 `modified`。
    #[serde(default = "default_date_source")]
    pub date_source: String,
    #[serde(default = "default_buffer_size")]
    pub buffer_size: u64,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub include_hidden: bool,
    #[serde(default = "default_true")]
    pub recursive: bool,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub verify: bool,
    #[serde(default = "default_verify_algorithm")]
    pub verify_algorithm: String,
    #[serde(default)]
    pub auto_rename: bool,
    #[serde(default = "default_workers")]
    pub workers: usize,
    #[serde(default)]
    pub fail_empty: bool,
    #[serde(default = "default_config_version")]
    pub config_version: u32,
    /// 旧配置里的原始策略名（`date` / `flat`），只用于提示，不落盘。
    #[serde(skip)]
    pub legacy_strategy: Option<String>,
    /// 构造/加载过程中产生的警告，由界面层显示。
    #[serde(skip)]
    pub warnings: Vec<String>,
}

fn default_strategy() -> String {
    "date".to_string()
}
fn default_date_source() -> String {
    "created".to_string()
}

fn default_date_format() -> String {
    "%Y/%m/%d".to_string()
}
fn default_buffer_size() -> u64 {
    DEFAULT_COPY_BUFSIZE
}
fn default_true() -> bool {
    true
}
fn default_verify_algorithm() -> String {
    "sha256".to_string()
}
fn default_workers() -> usize {
    1
}
fn default_config_version() -> u32 {
    CONFIG_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Self {
            source_dir: String::new(),
            target_dir: String::new(),
            extensions: Vec::new(),
            overwrite: false,
            strategy: default_strategy(),
            date_format: default_date_format(),
            date_source: default_date_source(),
            buffer_size: default_buffer_size(),
            exclude: Vec::new(),
            include_hidden: false,
            recursive: true,
            dry_run: false,
            verify: false,
            verify_algorithm: default_verify_algorithm(),
            auto_rename: false,
            workers: 1,
            fail_empty: false,
            config_version: CONFIG_VERSION,
            legacy_strategy: None,
            warnings: Vec::new(),
        }
    }
}

impl Config {
    /// 策略枚举视图；未知名字回退到 `date` 并记警告（与 Python `make_strategy` 一致）。
    pub fn strategy(&mut self) -> Strategy {
        match Strategy::from_name(&self.strategy) {
            Some(s) => s,
            None => {
                let name = self.strategy.clone();
                self.warnings
                    .push(format!("未知策略 '{name}'，使用默认日期策略"));
                self.strategy = Strategy::Date.name().to_string();
                Strategy::Date
            }
        }
    }

    pub fn src_path(&self) -> PathBuf {
        PathBuf::from(&self.source_dir)
    }

    pub fn dst_path(&self) -> PathBuf {
        PathBuf::from(&self.target_dir)
    }

    /// 展开 `~` / 环境变量（幂等）。所有入口都要过一遍。
    pub fn normalize_paths(&mut self) {
        self.source_dir = expand_user_path(&self.source_dir);
        self.target_dir = expand_user_path(&self.target_dir);
    }

    /// 语义校验（目录是否存在、是否自嵌套等）。错误列表为空表示可用。
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.source_dir.is_empty() {
            errors.push("源目录未设置".to_string());
        } else if !Path::new(&self.source_dir).is_dir() {
            errors.push(format!("源目录不存在: {}", self.source_dir));
        }
        if self.target_dir.is_empty() {
            errors.push("目标目录未设置".to_string());
        } else {
            let tgt = Path::new(&self.target_dir);
            if tgt.exists() && !tgt.is_dir() {
                // 不拦这条，会在建目录时抛 FileExistsError，用户看到一屏 traceback
                errors.push(format!("目标路径已存在但不是文件夹: {}", self.target_dir));
            }
        }
        if !self.source_dir.is_empty() && !self.target_dir.is_empty() {
            if let (Ok(src), Ok(tgt)) = (
                Path::new(&self.source_dir).canonicalize(),
                Path::new(&self.target_dir).canonicalize_allowing_missing(),
            ) {
                if src == tgt {
                    errors.push("源目录与目标目录相同".to_string());
                } else if tgt.starts_with(&src) {
                    errors.push("目标目录位于源目录内部，会导致递归复制".to_string());
                }
            }
        }
        if self.strategy == "date" || self.strategy == "date_tree" {
            if let Some(err) = validate_date_format(&self.date_format) {
                errors.push(err);
            }
            if crate::model::DateSource::from_name(&self.date_source).is_none() {
                errors.push(format!(
                    "日期来源非法: {}（可用: created, modified）",
                    self.date_source
                ));
            }
        }
        if self.verify && !VERIFY_ALGORITHMS.contains(&self.verify_algorithm.as_str()) {
            errors.push(format!(
                "校验算法非法: {}（可用: {}）",
                self.verify_algorithm,
                VERIFY_ALGORITHMS.join(", ")
            ));
        }
        if self.workers < 1 || self.workers > MAX_WORKERS {
            errors.push(format!(
                "并行线程数 workers 必须在 1..{MAX_WORKERS} 之间，当前 {}",
                self.workers
            ));
        }
        errors
    }

    pub fn to_json(&self) -> String {
        // serde_json 默认输出 UTF-8 原文，与 Python ensure_ascii=False 一致
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// 从 JSON 文本构造；schema 不合法时返回错误列表（不 panic）。
    pub fn from_json_text(text: &str) -> Result<Self, Vec<String>> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| vec![format!("配置文件不是合法 JSON: {e}")])?;
        Self::from_value(&value)
    }

    /// 从任意 Value 构造，先做严格 schema 校验，再做旧配置迁移。
    #[allow(clippy::field_reassign_with_default)] // 逐字段转换（每个 key 都要单独做类型/默认值处理），写成结构体字面量反而更难读
    pub fn from_value(value: &Value) -> Result<Self, Vec<String>> {
        let errors = validate_config_schema(value);
        if !errors.is_empty() {
            return Err(errors);
        }
        let obj = match value.as_object() {
            Some(o) => o.clone(),
            None => return Err(vec!["配置根节点必须是对象".to_string()]),
        };

        let mut config = Config::default();
        config.source_dir = as_str(&obj, "source_dir");
        config.target_dir = as_str(&obj, "target_dir");
        config.extensions = as_str_list(&obj, "extensions");
        config.exclude = as_str_list(&obj, "exclude");
        config.overwrite = as_bool(&obj, "overwrite", false);
        config.include_hidden = as_bool(&obj, "include_hidden", false);
        config.recursive = as_bool(&obj, "recursive", true);
        config.dry_run = as_bool(&obj, "dry_run", false);
        config.verify = as_bool(&obj, "verify", false);
        config.auto_rename = as_bool(&obj, "auto_rename", false);
        config.fail_empty = as_bool(&obj, "fail_empty", false);
        config.date_format = as_str_or(&obj, "date_format", &default_date_format());
        config.date_source = as_str_or(&obj, "date_source", &default_date_source());
        config.verify_algorithm = as_str_or(&obj, "verify_algorithm", &default_verify_algorithm());
        config.buffer_size = obj
            .get("buffer_size")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_COPY_BUFSIZE);
        config.workers = obj
            .get("workers")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(1);
        config.config_version = obj
            .get("config_version")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .unwrap_or(0);

        let requested = as_str_or(&obj, "strategy", &default_strategy());
        // 旧配置只有 date / flat / ext，且前两者都保留源目录结构
        if is_legacy_config(&obj) {
            if let Some(mapped) = match requested.as_str() {
                "date" => Some("date_tree"),
                "flat" => Some("tree"),
                _ => None,
            } {
                config.legacy_strategy = Some(requested.clone());
                config.strategy = mapped.to_string();
            } else {
                config.strategy = requested;
            }
        } else {
            config.strategy = requested;
        }
        config.config_version = CONFIG_VERSION;
        config.normalize_paths();
        Ok(config)
    }

    pub fn load_from_file(path: &Path) -> Result<Self, Vec<String>> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| vec![format!("读取配置失败: {}: {e}", path.display())])?;
        Self::from_json_text(&text)
    }

    pub fn save_to_file(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, self.to_json())
    }

    /// 旧配置被自动改写时的提示文案；没有改写返回 `None`。
    pub fn legacy_notice(&self) -> Option<String> {
        let old = self.legacy_strategy.as_ref()?;
        Some(format!(
            "这是旧版配置：strategy='{old}' 在旧版表示「保留源目录结构」，\
             已自动按 {} 执行。若确实要纯日期平铺，请把 strategy 改成 date 并加 \
             \"config_version\": 2。",
            self.strategy
        ))
    }
}

/// 读配置并做 schema 校验；语义校验留给调用方（界面会先用用户输入覆盖路径）。
pub fn load_config(path: &Path) -> Result<Config, Vec<String>> {
    Config::load_from_file(path)
}

/// 校验一份配置字典是否符合 schema，返回问题描述列表（空 = 合法）。
///
/// 纯函数，不碰文件系统，便于测试。注意 JSON 里 `true` 不是整数：
/// `buffer_size: true` / `workers: 1.5` 都必须报错，而不是静默取默认值。
pub fn validate_config_schema(data: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    let obj = match data.as_object() {
        Some(o) => o,
        None => return vec!["配置根节点必须是对象(dict)".to_string()],
    };

    for (key, kind) in CONFIG_FIELD_TYPES {
        let Some(value) = obj.get(*key) else { continue };
        let ok = match *kind {
            FieldKind::Text => value.is_string(),
            FieldKind::Bool => value.is_boolean(),
            FieldKind::Int => value.is_u64() || value.is_i64(),
            FieldKind::TextList => match value.as_array() {
                Some(items) => items.iter().all(Value::is_string),
                None => false,
            },
        };
        if !ok {
            errors.push(format!("字段 '{key}' 类型不正确: {value}"));
        }
    }

    if let Some(Value::String(strategy)) = obj.get("strategy") {
        if Strategy::from_name(strategy).is_none() {
            errors.push(format!(
                "字段 'strategy' 取值非法: {strategy:?}，可用: [\"date\", \"date_tree\", \"ext\", \"flat\", \"tree\"]"
            ));
        }
    }
    if let Some(v) = obj.get("buffer_size").and_then(Value::as_i64) {
        if v < 0 {
            errors.push(format!("字段 'buffer_size' 不能为负数: {v}"));
        }
    }
    if let Some(v) = obj.get("workers").and_then(Value::as_i64) {
        if v < 1 {
            errors.push(format!("字段 'workers' 至少为 1: {v}"));
        } else if v as u64 > MAX_WORKERS as u64 {
            errors.push(format!("字段 'workers' 不能超过 {MAX_WORKERS}: {v}"));
        }
    }
    if let Some(Value::String(algo)) = obj.get("verify_algorithm") {
        if !VERIFY_ALGORITHMS.contains(&algo.as_str()) {
            errors.push(format!(
                "字段 'verify_algorithm' 取值非法: {algo:?}，可用: {:?}",
                VERIFY_ALGORITHMS
            ));
        }
    }
    if let (Some(Value::String(fmt)), Some(Value::String(st))) =
        (obj.get("date_format"), obj.get("strategy"))
    {
        if st == "date" || st == "date_tree" {
            if let Some(err) = validate_date_format(fmt) {
                errors.push(format!("字段 'date_format': {err}"));
            }
        }
    }
    errors
}

/// 配置里认识的字段及其类型；同时用于「未知字段」提示。
static CONFIG_FIELD_TYPES: &[(&str, FieldKind)] = &[
    ("source_dir", FieldKind::Text),
    ("target_dir", FieldKind::Text),
    ("extensions", FieldKind::TextList),
    ("overwrite", FieldKind::Bool),
    ("strategy", FieldKind::Text),
    ("date_format", FieldKind::Text),
    ("date_source", FieldKind::Text),
    ("buffer_size", FieldKind::Int),
    ("exclude", FieldKind::TextList),
    ("include_hidden", FieldKind::Bool),
    ("recursive", FieldKind::Bool),
    ("dry_run", FieldKind::Bool),
    ("verify", FieldKind::Bool),
    ("verify_algorithm", FieldKind::Text),
    ("auto_rename", FieldKind::Bool),
    ("workers", FieldKind::Int),
    ("fail_empty", FieldKind::Bool),
];

#[derive(Debug, Clone, Copy)]
enum FieldKind {
    Text,
    Bool,
    Int,
    TextList,
}

/// 不认识的字段名（旧配置可能带额外字段：宽容，但不静默忽略）。
pub fn unknown_config_fields(obj: &Map<String, Value>) -> Vec<String> {
    let mut out: Vec<String> = obj
        .keys()
        .filter(|k| !CONFIG_FIELD_TYPES.iter().any(|(name, _)| name == k))
        .cloned()
        .collect();
    out.sort();
    out
}

/// 旧 `filecopy.py` 写出来的配置：没有 `config_version`，且字段集合被 7 个 key 包住。
///
/// 只看 `config_version` 不可靠：v0.3.0~v0.3.3 的配置也没有该字段，
/// 却已经在用新语义（flat = 真平铺）。
pub fn is_legacy_config(obj: &Map<String, Value>) -> bool {
    if obj
        .get("config_version")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        >= 2
    {
        return false;
    }
    obj.keys().all(|k| LEGACY_CONFIG_KEYS.contains(&k.as_str()))
}

/// 扫描目录下能被解析为 filecopy 配置的 `.json` 文件。
///
/// 判据放宽到「含 source_dir 或 target_dir 任一个 key」：用户手写配置时
/// 经常只填一半，不该因为缺字段就整个看不见。
pub fn scan_config_files(directory: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let entries = match std::fs::read_dir(directory) {
        Ok(e) => e,
        Err(_) => return result,
    };
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|e| e.eq_ignore_ascii_case("json"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if obj.contains_key("source_dir") || obj.contains_key("target_dir") {
            result.push(path);
        }
    }
    result
}

/// 把用户输入变成可直接使用的路径：去引号/空白、展开 `~` 与环境变量。
pub fn expand_user_path(raw: &str) -> String {
    let mut value = raw.trim();
    // 用户从 Finder「拷贝路径名」或 shell 里带引号粘贴都很常见
    value = value.trim_matches('"').trim_matches('\'').trim();
    if value.is_empty() {
        return String::new();
    }
    let expanded = expand_home(value);
    expand_env(&expanded)
}

fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|s| !s.is_empty())
}

fn expand_home(value: &str) -> String {
    let Some(home) = home_dir() else {
        return value.to_string();
    };
    if value == "~" {
        return home;
    }
    for prefix in ["~/", "~\\"] {
        if let Some(rest) = value.strip_prefix(prefix) {
            return format!("{home}/{rest}");
        }
    }
    value.to_string()
}

/// 展开 `$VAR` / `${VAR}`；未定义的变量原样保留（与 os.path.expandvars 一致）。
fn expand_env(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            if chars[i + 1] == '{' {
                if let Some(end) = chars[i + 2..].iter().position(|c| *c == '}') {
                    let name: String = chars[i + 2..i + 2 + end].iter().collect();
                    if let Ok(v) = std::env::var(&name) {
                        out.push_str(&v);
                        i += 3 + end;
                        continue;
                    }
                }
            } else if chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '_' {
                let start = i + 1;
                let mut end = start;
                while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_')
                {
                    end += 1;
                }
                let name: String = chars[start..end].iter().collect();
                if let Ok(v) = std::env::var(&name) {
                    out.push_str(&v);
                    i = end;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 解析用户填的扩展名：逗号/空格/分号都算分隔符，并容忍 `*.nef` 写法。
///
/// 只按逗号切分时，`nef cr2` 会被当成一个永远匹配不上的扩展名，
/// 静默零复制，用户以为备份成功。
pub fn parse_extensions(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        let mut t = token.trim();
        t = t.trim_start_matches('*');
        t = t.trim_start_matches('.');
        t = t.trim_end_matches('.');
        let t = t.to_lowercase();
        if !t.is_empty() {
            out.push(t);
        }
    }
    out
}

/// 扩展名集合去重（保持稳定顺序，便于界面上回显）。
pub fn dedup_extensions(items: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    items
        .into_iter()
        .filter(|e| seen.insert(e.clone()))
        .collect()
}

// ---------- Value 取值助手：schema 已保证类型，这里只做兜底 ----------

fn as_str(obj: &Map<String, Value>, key: &str) -> String {
    obj.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn as_str_or(obj: &Map<String, Value>, key: &str, fallback: &str) -> String {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

fn as_bool(obj: &Map<String, Value>, key: &str, fallback: bool) -> bool {
    obj.get(key).and_then(Value::as_bool).unwrap_or(fallback)
}

fn as_str_list(obj: &Map<String, Value>, key: &str) -> Vec<String> {
    obj.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `canonicalize` 的宽容版本：目标目录常常还不存在，此时退化成绝对化处理。
trait CanonicalizeAllowingMissing {
    fn canonicalize_allowing_missing(&self) -> std::io::Result<PathBuf>;
}

impl CanonicalizeAllowingMissing for Path {
    fn canonicalize_allowing_missing(&self) -> std::io::Result<PathBuf> {
        if self.exists() {
            return self.canonicalize();
        }
        // 逐级向上找到已存在的祖先，再把剩余部分拼回去（能展开软链与 `..`）
        let mut rest: Vec<std::ffi::OsString> = Vec::new();
        let mut cur = self.to_path_buf();
        loop {
            if cur.exists() {
                let mut base = cur.canonicalize()?;
                for part in rest.iter().rev() {
                    base.push(part);
                }
                return Ok(base);
            }
            match (cur.file_name(), cur.parent()) {
                (Some(name), Some(parent)) => {
                    rest.push(name.to_os_string());
                    cur = parent.to_path_buf();
                }
                _ => return Ok(self.to_path_buf()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // 测试里逐字段改配置最直观，不必为了消除 lint 写出巨大的结构体字面量
    #![allow(clippy::field_reassign_with_default)]

    use super::*;

    #[test]
    fn parses_extensions_like_python() {
        assert_eq!(parse_extensions("nef cr2"), vec!["nef", "cr2"]);
        assert_eq!(
            parse_extensions("*.NEF, .jpg; png"),
            vec!["nef", "jpg", "png"]
        );
        assert!(parse_extensions("  ,; ").is_empty());
    }

    #[test]
    fn expands_home_and_env() {
        std::env::set_var("FILECOPY_TEST_DIR", "/tmp/x");
        assert_eq!(expand_user_path("  \"$FILECOPY_TEST_DIR/a\"  "), "/tmp/x/a");
        assert!(expand_user_path("~/Pics").starts_with('/'));
        assert_eq!(expand_user_path(""), "");
    }

    #[test]
    fn legacy_config_maps_strategies() {
        let raw = r#"{
            "source_dir": "/a", "target_dir": "/b",
            "strategy": "flat", "extensions": [], "overwrite": false,
            "date_format": "%Y/%m/%d", "buffer_size": 1048576
        }"#;
        let cfg = Config::from_json_text(raw).expect("合法旧配置");
        assert_eq!(cfg.strategy, "tree");
        assert_eq!(cfg.legacy_strategy.as_deref(), Some("flat"));
        assert!(cfg.legacy_notice().is_some());

        let raw_date = raw.replace("\"flat\"", "\"date\"");
        let cfg = Config::from_json_text(&raw_date).unwrap();
        assert_eq!(cfg.strategy, "date_tree");
    }

    #[test]
    fn new_config_keeps_semantics() {
        let raw = r#"{
            "source_dir": "/a", "target_dir": "/b",
            "strategy": "flat", "config_version": 2
        }"#;
        let cfg = Config::from_json_text(raw).unwrap();
        assert_eq!(cfg.strategy, "flat");
        assert!(cfg.legacy_strategy.is_none());
    }

    #[test]
    fn schema_rejects_bad_types_and_ranges() {
        let bad: Value = serde_json::from_str(
            r#"{"overwrite": 1, "workers": 0, "buffer_size": -1,
                "strategy": "nope", "verify_algorithm": "crc32",
                "extensions": [1], "date_format": "%Q"}"#,
        )
        .unwrap();
        let errors = validate_config_schema(&bad);
        assert!(errors.len() >= 6, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("overwrite")));
        assert!(errors.iter().any(|e| e.contains("workers")));
        assert!(errors.iter().any(|e| e.contains("buffer_size")));
    }

    #[test]
    fn roundtrip_json() {
        let mut cfg = Config::default();
        cfg.source_dir = "/a".into();
        cfg.target_dir = "/b".into();
        cfg.strategy = "date_tree".into();
        let text = cfg.to_json();
        let back = Config::from_json_text(&text).unwrap();
        assert_eq!(back.strategy, "date_tree");
        assert_eq!(back.buffer_size, DEFAULT_COPY_BUFSIZE);
        assert!(back.recursive);
    }
}
