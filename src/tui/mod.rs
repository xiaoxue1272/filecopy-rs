//! 交互界面：单屏状态机（表单 / 目录浏览 / 配置选择 / 保存命名 / 运行）。
//!
//! 为什么是 TUI 而不是 CLI 参数：用户明确要求「全部走 TUI」，
//! 路径、策略、开关都能上下键选，复制记录是可视化可滚动列表。

mod render;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::config::{scan_config_files, Config};
use crate::engine::{Engine, EngineEvent, Summary};
use crate::model::{
    format_size, CopyRecord, DateSource, FileAction, Strategy, EXIT_SIGINT, VERIFY_ALGORITHMS,
    VERSION,
};

/// 外部信号（SIGINT / SIGTERM / SIGHUP）到达时置位。
///
/// 信号处理函数里只允许做最简单的原子写：真正的收尾在主循环下一轮做，
/// 这样既不会在 handler 里触碰锁，也不会把在途临时文件丢下。
static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// 表单字段顺序（也是 Tab 的移动顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Source,
    Target,
    Extensions,
    Exclude,
    Strategy,
    DateFormat,
    /// 日期策略取哪个时间戳：创建日期 / 修改日期。
    DateSource,
    /// 同名文件怎么处理：跳过 / 覆盖 / 自动改名（三选一，取代原来的两个开关）。
    Conflict,
    Verify,
    VerifyAlgo,
    IncludeHidden,
    Recursive,
    FailEmpty,
    Workers,
    /// 复制缓冲区大小（0 = 默认）。
    BufferSize,
    /// 「高级选项」展开/收起开关，本身不写进配置。
    Advanced,
    /// 启动按钮：只按计划预览，不写文件。
    Preview,
    /// 启动按钮：真正开始复制。
    Start,
}

impl Field {
    /// 顺序必须和屏幕上的自上而下一致，否则 ↓ 会跳过中间的行、
    /// 逼着用户绕一整圈才能走到可见的分区里。
    pub const ALL: [Field; 18] = [
        // 目录与过滤
        Field::Source,
        Field::Target,
        Field::Extensions,
        Field::Exclude,
        // 性能
        Field::Workers,
        Field::BufferSize,
        // 更多选项 → 高级选项
        Field::Advanced,
        // 策略与开关（只在展开后可见，位置就在「高级选项」下面）
        Field::Strategy,
        Field::DateFormat,
        Field::DateSource,
        Field::Conflict,
        Field::Verify,
        Field::VerifyAlgo,
        Field::IncludeHidden,
        Field::Recursive,
        Field::FailEmpty,
        // 操作
        Field::Start,
        Field::Preview,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::Source => "源目录",
            Field::Target => "目标目录",
            Field::Extensions => "扩展名",
            Field::Exclude => "排除规则",
            Field::Strategy => "分类策略",
            Field::DateFormat => "日期格式",
            Field::DateSource => "日期来源",
            Field::Conflict => "同名文件",
            Field::Verify => "校验和",
            Field::VerifyAlgo => "校验算法",
            Field::IncludeHidden => "包含隐藏",
            Field::Recursive => "递归子目录",
            Field::FailEmpty => "空扫描算失败",
            Field::Workers => "线程数",
            Field::BufferSize => "缓冲区",
            Field::Advanced => "高级选项",
            Field::Preview => "预览",
            Field::Start => "开始",
        }
    }

    /// 默认收在「高级选项」里的字段：策略、校验、冲突处理、隐藏文件等。
    pub fn is_advanced_group(self) -> bool {
        matches!(
            self,
            Field::Strategy
                | Field::DateFormat
                | Field::DateSource
                | Field::Conflict
                | Field::Verify
                | Field::VerifyAlgo
                | Field::IncludeHidden
                | Field::Recursive
                | Field::FailEmpty
        )
    }

    pub fn is_text(self) -> bool {
        matches!(
            self,
            Field::Source
                | Field::Target
                | Field::Extensions
                | Field::Exclude
                | Field::DateFormat
                | Field::Workers
                | Field::BufferSize
        )
    }

    pub fn is_bool(self) -> bool {
        matches!(
            self,
            Field::Verify | Field::IncludeHidden | Field::Recursive | Field::FailEmpty
        )
    }

    /// 枚举型：空格/回车在里面循环取值。
    pub fn is_choice(self) -> bool {
        matches!(
            self,
            Field::Strategy | Field::VerifyAlgo | Field::Conflict | Field::DateSource
        )
    }

    pub fn is_path(self) -> bool {
        matches!(self, Field::Source | Field::Target)
    }

    /// 启动按钮：选中后回车或空格触发，本身不是配置项。
    pub fn is_action(self) -> bool {
        matches!(self, Field::Preview | Field::Start)
    }
}

/// 表单的可编辑数据（与 `Config` 互转，保持用户输入原样）。
#[derive(Debug, Clone)]
pub struct FormData {
    pub source: String,
    pub target: String,
    pub extensions: String,
    pub exclude: String,
    pub strategy_idx: usize,
    pub date_format: String,
    pub date_source_idx: usize,
    /// CONFLICT_POLICIES 的下标：0 跳过、1 覆盖、2 自动改名。
    pub conflict_idx: usize,
    pub verify: bool,
    pub verify_algo_idx: usize,
    pub include_hidden: bool,
    pub recursive: bool,
    pub fail_empty: bool,
    pub workers: String,
    /// 只是数字部分，单位在 `buffer_unit_idx` 里。
    pub buffer_size: String,
    pub buffer_unit_idx: usize,
    /// 「策略与开关」是否展开；只是界面状态，不写进配置。
    pub advanced_open: bool,
}

impl Default for FormData {
    fn default() -> Self {
        Self {
            source: String::new(),
            target: String::new(),
            extensions: String::new(),
            exclude: String::new(),
            strategy_idx: 0,
            date_format: "%Y/%m/%d".to_string(),
            date_source_idx: 0,
            conflict_idx: 0,
            verify: false,
            verify_algo_idx: 2, // sha256
            include_hidden: false,
            recursive: true,
            fail_empty: false,
            workers: "1".to_string(),
            buffer_size: "1".to_string(),
            buffer_unit_idx: 2, // 默认 1 MB，和 DEFAULT_COPY_BUFSIZE 一致
            advanced_open: false,
        }
    }
}

/// 从配置里的两个布尔反推界面上那个三选一。
fn conflict_idx_of(cfg: &Config) -> usize {
    if cfg.auto_rename {
        2
    } else if cfg.overwrite {
        1
    } else {
        0
    }
}

/// 并排显示的字段对（左, 右）。渲染和左右键换列都用这份定义，避免两处各写一遍。
pub const FIELD_PAIRS: [(Field, Field); 2] = [
    (Field::Verify, Field::VerifyAlgo),
    (Field::IncludeHidden, Field::Recursive),
];

/// 缓冲区大小的单位；数字 × 这个倍数就是字节数。
pub const BUFFER_UNITS: [(&str, u64); 4] = [
    ("B", 1),
    ("KB", 1024),
    ("MB", 1024 * 1024),
    ("GB", 1024 * 1024 * 1024),
];

/// 把配置里的字节数拆成「数字 + 尽量大的整数单位」，1 MiB 显示成 `1 MB` 而不是 `1048576 B`。
fn split_buffer_size(bytes: u64) -> (String, usize) {
    for (idx, (_, factor)) in BUFFER_UNITS.iter().enumerate().rev() {
        if *factor > 1 && bytes >= *factor && bytes.is_multiple_of(*factor) {
            return ((bytes / factor).to_string(), idx);
        }
    }
    (bytes.to_string(), 0)
}

/// 同名文件处理的三种策略（写回配置时映射成 overwrite / auto_rename 两个布尔）。
pub const CONFLICT_POLICIES: [&str; 3] = ["跳过", "覆盖", "自动改名"];

/// 两个启动按钮并排显示，左＝开始，右＝预览。
pub const ACTION_PAIR: (Field, Field) = (Field::Start, Field::Preview);

impl Field {
    /// 若该字段和另一个字段并排，返回（左字段, 右字段）。
    fn pair(self) -> Option<(Field, Field)> {
        if self.is_action() {
            return Some(ACTION_PAIR);
        }
        FIELD_PAIRS
            .iter()
            .copied()
            .find(|(l, r)| *l == self || *r == self)
    }
}

impl FormData {
    /// 当前策略是否按日期分目录（只有这类策略用得上日期格式）。
    pub fn uses_date_format(&self) -> bool {
        let index = self.strategy_idx.min(Strategy::ALL.len() - 1);
        matches!(Strategy::ALL[index], Strategy::Date | Strategy::DateTree)
    }

    /// 字段现在是否可见：依赖项没开的字段直接藏起来，不让用户去改无意义的选项。
    pub fn field_visible(&self, field: Field) -> bool {
        // 依赖项只在「高级选项」展开时才谈得上显示：收起时哪怕策略是按日期、
        // 校验是开着的，这两行也不在屏幕上，光标不能落到它们上面。
        match field {
            Field::DateFormat => self.advanced_open && self.uses_date_format(),
            Field::DateSource => self.advanced_open && self.uses_date_format(),
            Field::VerifyAlgo => self.advanced_open && self.verify,
            other if other.is_advanced_group() => self.advanced_open,
            _ => true,
        }
    }

    /// 上下键真正会经过的字段序列。
    /// 屏幕上的行：(左, 右)。单行字段右边是 None；并排行里被依赖项藏起来的也算 None。
    pub fn form_rows(&self) -> Vec<(Option<Field>, Option<Field>)> {
        let mut rows: Vec<(Option<Field>, Option<Field>)> = Vec::new();
        for field in Field::ALL {
            if !self.field_visible(field) {
                continue;
            }
            match field.pair() {
                Some((left, right)) => {
                    if field == right {
                        // 右列已经作为左列的同伴登记过了
                        continue;
                    }
                    let right = if self.field_visible(right) {
                        Some(right)
                    } else {
                        None
                    };
                    rows.push((Some(left), right));
                }
                None => {
                    if field.is_action() {
                        continue;
                    }
                    rows.push((Some(field), None));
                }
            }
        }
        rows
    }

    pub fn visible_fields(&self) -> Vec<Field> {
        Field::ALL
            .iter()
            .copied()
            .filter(|field| self.field_visible(*field))
            .collect()
    }

    pub fn from_config(cfg: &Config) -> Self {
        let strategy_idx = Strategy::ALL
            .iter()
            .position(|s| s.name() == cfg.strategy)
            .unwrap_or(0);
        let verify_algo_idx = VERIFY_ALGORITHMS
            .iter()
            .position(|a| *a == cfg.verify_algorithm)
            .unwrap_or(2);
        Self {
            source: cfg.source_dir.clone(),
            target: cfg.target_dir.clone(),
            extensions: cfg.extensions.join(", "),
            exclude: cfg.exclude.join(", "),
            strategy_idx,
            date_format: cfg.date_format.clone(),
            date_source_idx: DateSource::from_name(&cfg.date_source)
                .map(|source| {
                    DateSource::ALL
                        .iter()
                        .position(|s| *s == source)
                        .unwrap_or(0)
                })
                .unwrap_or(0),
            conflict_idx: conflict_idx_of(cfg),
            verify: cfg.verify,
            verify_algo_idx,
            include_hidden: cfg.include_hidden,
            recursive: cfg.recursive,
            fail_empty: cfg.fail_empty,
            workers: cfg.workers.max(1).to_string(),
            buffer_size: split_buffer_size(cfg.buffer_size).0,
            buffer_unit_idx: split_buffer_size(cfg.buffer_size).1,
            advanced_open: false,
        }
    }

    /// 转成 `Config`。扩展名/排除规则在这里做宽松解析，非法数字退回默认值。
    #[allow(clippy::field_reassign_with_default)] // 从表单逐字段搬运，比一个巨大的结构体字面量好读
    pub fn to_config(&self) -> Config {
        let mut cfg = Config::default();
        cfg.source_dir = self.source.clone();
        cfg.target_dir = self.target.clone();
        cfg.extensions = crate::config::parse_extensions(&self.extensions);
        cfg.exclude = self
            .exclude
            .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        cfg.strategy = Strategy::ALL[self.strategy_idx.min(Strategy::ALL.len() - 1)]
            .name()
            .to_string();
        cfg.date_format = self.date_format.clone();
        // 界面上是一个三选一，写进配置仍是两个布尔：覆盖 / 自动改名
        cfg.overwrite = self.conflict_idx == 1;
        cfg.auto_rename = self.conflict_idx == 2;
        cfg.verify = self.verify;
        cfg.verify_algorithm =
            VERIFY_ALGORITHMS[self.verify_algo_idx.min(VERIFY_ALGORITHMS.len() - 1)].to_string();
        cfg.date_source = self.date_source().name().to_string();
        cfg.include_hidden = self.include_hidden;
        cfg.recursive = self.recursive;
        cfg.fail_empty = self.fail_empty;
        cfg.workers = self.workers.trim().parse::<usize>().unwrap_or(1).max(1);
        cfg.buffer_size = self
            .buffer_bytes()
            .unwrap_or(crate::config::DEFAULT_COPY_BUFSIZE);
        cfg.normalize_paths();
        cfg
    }

    /// 文本字段的可变引用。
    pub fn text_mut(&mut self, field: Field) -> Option<&mut String> {
        match field {
            Field::Source => Some(&mut self.source),
            Field::Target => Some(&mut self.target),
            Field::Extensions => Some(&mut self.extensions),
            Field::Exclude => Some(&mut self.exclude),
            Field::DateFormat => Some(&mut self.date_format),
            Field::Workers => Some(&mut self.workers),
            Field::BufferSize => Some(&mut self.buffer_size),
            _ => None,
        }
    }

    /// 当前选中的日期来源。
    pub fn date_source(&self) -> DateSource {
        DateSource::ALL[self.date_source_idx.min(DateSource::ALL.len() - 1)]
    }

    /// 「扩展名」里填的第一个，用来给示例落点编文件名。
    pub fn first_extension(&self) -> Option<String> {
        crate::config::parse_extensions(&self.extensions)
            .into_iter()
            .next()
    }

    /// 数字 × 单位 = 字节数；`0` 交给引擎按默认处理。
    pub fn buffer_bytes(&self) -> Option<u64> {
        let value = self.buffer_size.trim().parse::<u64>().ok()?;
        let factor = BUFFER_UNITS[self.buffer_unit_idx.min(BUFFER_UNITS.len() - 1)].1;
        Some(value.saturating_mul(factor))
    }

    pub fn text(&self, field: Field) -> String {
        match field {
            Field::Source => self.source.clone(),
            Field::Target => self.target.clone(),
            Field::Extensions => self.extensions.clone(),
            Field::Exclude => self.exclude.clone(),
            Field::DateFormat => self.date_format.clone(),
            Field::Workers => self.workers.clone(),
            Field::BufferSize => self.buffer_size.clone(),
            Field::DateSource => self.date_source().ui_label().to_string(),
            Field::Strategy => Strategy::ALL[self.strategy_idx].name().to_string(),
            Field::VerifyAlgo => VERIFY_ALGORITHMS[self.verify_algo_idx].to_string(),
            Field::Conflict => {
                CONFLICT_POLICIES[self.conflict_idx.min(CONFLICT_POLICIES.len() - 1)].to_string()
            }
            other => self
                .bool_of(other)
                .map(|b| if b { "开" } else { "关" })
                .unwrap_or_default()
                .to_string(),
        }
    }

    pub fn bool_of(&self, field: Field) -> Option<bool> {
        Some(match field {
            Field::Verify => self.verify,
            Field::IncludeHidden => self.include_hidden,
            Field::Recursive => self.recursive,
            Field::FailEmpty => self.fail_empty,
            _ => return None,
        })
    }

    fn bool_mut(&mut self, field: Field) -> Option<&mut bool> {
        Some(match field {
            Field::Verify => &mut self.verify,
            Field::IncludeHidden => &mut self.include_hidden,
            Field::Recursive => &mut self.recursive,
            Field::FailEmpty => &mut self.fail_empty,
            _ => return None,
        })
    }
}

/// 目录浏览器的临时状态。
pub struct BrowserState {
    pub field: Field,
    pub dir: PathBuf,
    pub entries: Vec<BrowserEntry>,
    pub selected: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum BrowserEntry {
    Parent,
    ChooseHere,
    Dir(PathBuf),
}

impl BrowserEntry {
    pub fn label(&self) -> String {
        match self {
            BrowserEntry::Parent => ".. 上一级".to_string(),
            BrowserEntry::ChooseHere => "[ 使用当前目录 ]".to_string(),
            BrowserEntry::Dir(p) => format!(
                "{}  {}/",
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                // 末尾斜杠只是视觉提示「这是目录」
                ""
            ),
        }
    }
}

/// 配置选择器。
pub struct PickerState {
    pub files: Vec<PathBuf>,
    pub selected: usize,
}

/// 保存配置时的文件名输入。
pub struct PromptState {
    pub input: String,
    pub cursor: usize,
    pub error: Option<String>,
}

/// 运行界面状态。
pub struct RunState {
    pub rx: Receiver<EngineEvent>,
    pub handle: Option<std::thread::JoinHandle<()>>,
    pub cancel: Arc<AtomicBool>,
    pub records: Vec<CopyRecord>,
    pub logs: Vec<String>,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub summary: Option<Summary>,
    pub scroll: usize,
    pub follow: bool,
}

impl RunState {
    fn push_log(&mut self, line: String) {
        self.logs.push(line);
        // 日志只留最近 200 行：内存里不需要保留整场复制的每一行
        if self.logs.len() > 200 {
            let drop = self.logs.len() - 200;
            self.logs.drain(0..drop);
        }
    }

    fn push_record(&mut self, rec: CopyRecord) {
        self.records.push(rec);
        if self.follow {
            self.scroll = self.records.len().saturating_sub(1);
        }
    }
}

/// 界面模式。
pub enum Mode {
    Form,
    Browser(BrowserState),
    Picker(PickerState),
    Prompt(PromptState),
    Run(RunState),
}

/// 应用主体。
pub struct App {
    pub form: FormData,
    pub field: Field,
    pub cursor: usize,
    pub mode: Mode,
    pub status: String,
    pub errors: Vec<String>,
    pub should_quit: bool,
    /// 本次退出是否由外部信号触发（决定退出码）
    pub signalled: bool,
    pub last_exit: i32,
    pub cwd: PathBuf,
    /// 目录浏览器的滚动偏移（长目录列表用）。
    pub browser_scroll: usize,
    pub picker_scroll: usize,
}

impl App {
    pub fn new(initial: Option<Config>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut app = Self {
            form: initial
                .as_ref()
                .map(FormData::from_config)
                .unwrap_or_default(),
            field: Field::Source,
            cursor: 0,
            mode: Mode::Form,
            status: String::new(),
            errors: Vec::new(),
            should_quit: false,
            signalled: false,
            last_exit: 0,
            cwd,
            browser_scroll: 0,
            picker_scroll: 0,
        };
        if let Some(cfg) = initial {
            if let Some(notice) = cfg.legacy_notice() {
                app.status = notice;
            }
        }
        app
    }

    /// 上下移动：一「行」是一个并排组，尽量停在原来那一列上。
    ///
    /// 以前是直接在一维列表里前后跳，从右列往下会跳到下一行的左列，
    /// 用起来像光标自己跑偏了。
    fn move_field(&mut self, delta: isize) {
        let rows = self.form.form_rows();
        if rows.is_empty() {
            return;
        }
        let current = rows
            .iter()
            .position(|(left, right)| *left == Some(self.field) || *right == Some(self.field))
            .unwrap_or(0);
        // 当前停在右列吗？跟着往下走就该继续待在右列
        let stay_right = rows[current].1 == Some(self.field);
        let len = rows.len() as isize;
        let next = (current as isize + delta).rem_euclid(len) as usize;
        let (left, right) = rows[next];
        let target = match (stay_right, right) {
            (true, Some(right)) => right,
            _ => left.or(right).unwrap_or(self.field),
        };
        self.focus(target);
    }

    /// 左右键换列：只在并排的两个字段之间移动，栏内没有伙伴就不动。
    fn move_horizontal(&mut self, to_right: bool) {
        let Some((left, right)) = self.field.pair() else {
            return;
        };
        let target = match (to_right, self.field == left) {
            (true, true) => right,
            (false, false) => left,
            _ => return,
        };
        if !self.form.field_visible(target) {
            return;
        }
        self.focus(target);
    }

    fn focus(&mut self, field: Field) {
        self.field = field;
        self.cursor = self.form.text(field).chars().count();
        self.errors.clear();
    }

    /// 当前字段因为依赖项变化而消失时（关掉校验、切到不按日期的策略、收起高级选项），
    /// 把光标挪回一个还能看见的字段，避免停在隐藏项上按什么都没反应。
    fn normalize_field(&mut self) {
        if self.form.field_visible(self.field) {
            return;
        }
        // 高级选项里的行被藏起来时，退回「高级选项」那一行，别把人扔到表单开头
        if self.field.is_advanced_group() && self.form.field_visible(Field::Advanced) {
            self.focus(Field::Advanced);
            return;
        }
        if let Some((left, _right)) = self.field.pair() {
            if self.form.field_visible(left) {
                self.focus(left);
                return;
            }
        }
        let fields = self.form.visible_fields();
        if let Some(first) = fields.first().copied() {
            self.focus(first);
        }
    }

    fn toggle_advanced(&mut self) {
        self.form.advanced_open = !self.form.advanced_open;
        // 展开后光标直接进入新出现的第一行，收起后退回「高级选项」这一行，
        // 不然光标会卡在开关行上、得绕一圈才能进去。
        if self.form.advanced_open {
            self.focus(Field::Strategy);
        } else {
            self.focus(Field::Advanced);
        }
    }

    fn toggle_bool(&mut self) {
        if let Some(b) = self.form.bool_mut(self.field) {
            *b = !*b;
        }
        self.normalize_field();
    }

    fn cycle(&mut self, forward: bool) {
        if self.field == Field::BufferSize {
            let len = BUFFER_UNITS.len();
            self.form.buffer_unit_idx =
                (self.form.buffer_unit_idx + if forward { 1 } else { len - 1 }) % len;
            return;
        }
        match self.field {
            Field::Strategy => {
                let len = Strategy::ALL.len();
                self.form.strategy_idx =
                    (self.form.strategy_idx + if forward { 1 } else { len - 1 }) % len;
            }
            Field::VerifyAlgo => {
                let len = VERIFY_ALGORITHMS.len();
                self.form.verify_algo_idx =
                    (self.form.verify_algo_idx + if forward { 1 } else { len - 1 }) % len;
            }
            Field::DateSource => {
                let len = DateSource::ALL.len();
                self.form.date_source_idx =
                    (self.form.date_source_idx + if forward { 1 } else { len - 1 }) % len;
            }
            Field::Conflict => {
                let len = CONFLICT_POLICIES.len();
                self.form.conflict_idx =
                    (self.form.conflict_idx + if forward { 1 } else { len - 1 }) % len;
            }
            Field::DateFormat => {
                // 日期格式有预设序列，用户也可以直接手打
                let presets: Vec<&str> =
                    crate::model::DATE_FORMATS.iter().map(|(_, f)| *f).collect();
                let pos = presets.iter().position(|f| *f == self.form.date_format);
                let next = match (pos, forward) {
                    (Some(i), true) => (i + 1) % presets.len(),
                    (Some(i), false) => (i + presets.len() - 1) % presets.len(),
                    (None, true) => 0,
                    (None, false) => presets.len() - 1,
                };
                self.form.date_format = presets[next].to_string();
            }
            _ => {}
        }
        self.normalize_field();
    }

    fn handle_text_key(&mut self, key: &KeyEvent) {
        let chars_len = self.form.text(self.field).chars().count();
        // 光标可能来自「更长的」旧内容（选目录、加载配置都会直接换掉文本），
        // 先夹回本字段范围内，否则 Backspace/Delete 会按越界下标删字符而 panic。
        if self.cursor > chars_len {
            self.cursor = chars_len;
        }
        match key.code {
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(chars_len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = chars_len,
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    if let Some(text) = self.form.text_mut(self.field) {
                        let idx = byte_index(text, self.cursor - 1);
                        if idx < text.len() {
                            text.remove(idx);
                        }
                    }
                    self.cursor -= 1;
                }
            }
            KeyCode::Delete => {
                if self.cursor < chars_len {
                    if let Some(text) = self.form.text_mut(self.field) {
                        let idx = byte_index(text, self.cursor);
                        if idx < text.len() {
                            text.remove(idx);
                        }
                    }
                }
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return;
                }
                // 缓冲区只收数字：单位是拿空格换的，混进字母只会变成一条校验错误
                if self.field == Field::BufferSize && !c.is_ascii_digit() {
                    return;
                }
                if let Some(text) = self.form.text_mut(self.field) {
                    let idx = byte_index(text, self.cursor);
                    if idx <= text.len() {
                        text.insert(idx, c);
                    }
                }
                self.cursor += 1;
            }
            _ => {}
        }
    }

    /// 校验表单；返回错误列表（空 = 可以开始）。
    fn validate_form(&mut self) -> Vec<String> {
        let cfg = self.form.to_config();
        let mut errors = cfg.validate();
        if !cfg.source_dir.is_empty() && !Path::new(&cfg.source_dir).is_dir() {
            errors.push(format!("源目录不存在: {}", cfg.source_dir));
        }
        if self.form.workers.trim().parse::<usize>().is_err() {
            errors.push(format!("线程数必须是数字: {}", self.form.workers));
        }
        match self.form.buffer_bytes() {
            None => errors.push(format!(
                "缓冲区大小必须是不小于 0 的整数: {}",
                self.form.buffer_size
            )),
            Some(0) => {}
            Some(bytes) if !(4096..=64 * 1024 * 1024).contains(&bytes) => {
                errors.push(format!(
                    "缓冲区需在 4 KB 到 64 MB 之间（现在是 {bytes} 字节），0 = 默认"
                ));
            }
            Some(_) => {}
        }
        if cfg.verify && !VERIFY_ALGORITHMS.contains(&cfg.verify_algorithm.as_str()) {
            errors.push(format!("校验算法非法: {}", cfg.verify_algorithm));
        }
        errors
    }

    /// 启动一次运行：`dry_run` 为真时只预览、不写文件。
    /// 这个状态只属于本次运行，表单里不再有「仅预览」开关。
    fn start(&mut self, dry_run: bool) {
        let errors = self.validate_form();
        if !errors.is_empty() {
            self.errors = errors;
            return;
        }
        let mut cfg = self.form.to_config();
        cfg.dry_run = dry_run;
        let cancel = Arc::new(AtomicBool::new(false));
        let (mut engine, rx) = Engine::new(cfg.clone(), cancel.clone());
        let handle = std::thread::spawn(move || {
            engine.run();
        });
        self.mode = Mode::Run(RunState {
            rx,
            handle: Some(handle),
            cancel,
            records: Vec::new(),
            logs: Vec::new(),
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            summary: None,
            scroll: 0,
            follow: true,
        });
        self.errors.clear();
        self.status.clear();
    }

    /// 把运行线程的事件搬到界面状态里（每个 tick 调一次）。
    fn pump_run(&mut self) {
        let Mode::Run(run) = &mut self.mode else {
            return;
        };
        loop {
            match run.rx.try_recv() {
                Ok(EngineEvent::Log(line)) => run.push_log(line),
                Ok(EngineEvent::Record(rec)) => run.push_record(rec),
                Ok(EngineEvent::Progress {
                    files_done,
                    files_total,
                    bytes_done,
                    bytes_total,
                }) => {
                    run.files_done = files_done;
                    run.files_total = files_total;
                    run.bytes_done = bytes_done;
                    run.bytes_total = bytes_total;
                }
                Ok(EngineEvent::Finished(summary)) => {
                    run.summary = Some(summary);
                }
                Err(_) => break,
            }
        }
    }

    /// 打开目录浏览器（源/目标字段）。
    fn open_browser(&mut self) {
        let current = self.form.text(self.field);
        let start = if !current.is_empty() && Path::new(&current).is_dir() {
            PathBuf::from(&current)
        } else {
            self.cwd.clone()
        };
        let mut browser = BrowserState {
            field: self.field,
            dir: start,
            entries: Vec::new(),
            selected: 0,
            error: None,
        };
        reload_browser(&mut browser);
        self.browser_scroll = 0;
        self.mode = Mode::Browser(browser);
    }

    fn open_picker(&mut self) {
        let files = scan_config_files(&self.cwd);
        if files.is_empty() {
            self.status = format!("当前目录没有找到 filecopy 配置: {}", self.cwd.display());
            return;
        }
        self.picker_scroll = 0;
        self.mode = Mode::Picker(PickerState { files, selected: 0 });
    }

    fn open_prompt(&mut self) {
        self.mode = Mode::Prompt(PromptState {
            input: "filecopy.json".to_string(),
            cursor: "filecopy.json".chars().count(),
            error: None,
        });
    }
}

fn byte_index(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

fn reload_browser(browser: &mut BrowserState) {
    browser.entries.clear();
    browser.entries.push(BrowserEntry::Parent);
    browser.entries.push(BrowserEntry::ChooseHere);
    browser.error = None;
    match std::fs::read_dir(&browser.dir) {
        Ok(entries) => {
            let mut dirs: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .filter(|p| {
                    // 只列非隐藏子目录，与 Python 版浏览器一致
                    p.file_name()
                        .map(|n| !n.to_string_lossy().starts_with('.'))
                        .unwrap_or(false)
                })
                .collect();
            dirs.sort();
            for d in dirs {
                browser.entries.push(BrowserEntry::Dir(d));
            }
        }
        Err(e) => {
            browser.error = Some(format!("无法读取目录: {e}"));
        }
    }
    browser.selected = 0;
}

/// 出任何 panic 时先把终端恢复原样，再让默认处理打印信息。
///
/// 少了这层保护，一次崩溃会把终端留在 raw mode + 备用屏幕里：
/// 界面停在最后一帧、按键没有任何反应，看起来就像「卡死退不出去」。
/// 把终端还给用户：离开备用屏幕、恢复行模式、显回光标。幂等，失败也不报错。
fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    let _ = execute!(out, LeaveAlternateScreen, crossterm::cursor::Show);
    let _ = io::Write::flush(&mut out);
}

/// panic 时也必须还终端，否则进程死了但终端还卡在 raw mode + 备用屏幕里，
/// 用户看到的是「界面定格、按键没反应」。
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
}

/// 运行 TUI；返回进程退出码。
pub fn run(initial: Option<Config>) -> io::Result<i32> {
    install_panic_hook();
    // 被 kill / 关终端时也要走中断收尾；设置失败（例如已装过）不影响界面
    let _ = ctrlc::set_handler(|| SIGNALLED.store(true, Ordering::SeqCst));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(err) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(err);
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(err) => {
            restore_terminal();
            return Err(err);
        }
    };
    if let Err(err) = terminal.hide_cursor() {
        restore_terminal();
        return Err(err);
    }

    let mut app = App::new(initial);
    let result = event_loop(&mut terminal, &mut app);

    restore_terminal();
    result?;
    Ok(app.last_exit)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| render::draw(f, app))?;
        app.pump_run();
        if SIGNALLED.swap(false, Ordering::SeqCst) {
            on_signal(app);
        }

        if event::poll(Duration::from_millis(60))? {
            if let Event::Key(key) = event::read()? {
                handle_key(app, key);
            }
        }
        if app.should_quit {
            // 运行中退出：先请求中断，再等后台线程把临时文件清干净，别留在半路
            if let Mode::Run(run) = &mut app.mode {
                run.cancel.store(true, Ordering::Relaxed);
                if let Some(handle) = run.handle.take() {
                    let _ = handle.join();
                }
            }
            break;
        }
        if let Mode::Run(run) = &app.mode {
            if run.summary.is_some() && run.handle.as_ref().is_some_and(|h| h.is_finished()) {
                app.last_exit = run
                    .summary
                    .as_ref()
                    .map(|s| s.exit_code(app.form.fail_empty))
                    .unwrap_or(0);
                // 被信号打断的这次运行：收尾完成后直接退出，不留在一个「已完成」的界面上
                if app.signalled {
                    app.should_quit = true;
                }
            }
        }
    }
    Ok(())
}

/// 信号到达：复制中请求中断并等它收尾，否则立即退出。
fn on_signal(app: &mut App) {
    app.signalled = true;
    match &mut app.mode {
        Mode::Run(run) => {
            run.cancel.store(true, Ordering::Relaxed);
            run.push_log("收到终止信号：正在中断并清理在途临时文件…".to_string());
        }
        _ => {
            app.last_exit = EXIT_SIGINT;
            app.should_quit = true;
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // 有些终端会同时上报按下和松开事件（增强键盘协议）；重复处理会让一次按键变成两个动作，
    // 例如 Esc 先关弹窗、紧接着又触发「退出程序」。这里只认按下与自动重复。
    if key.kind == KeyEventKind::Release {
        return;
    }

    // 全局退出：Ctrl-C / Ctrl-Q
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q'))
    {
        app.should_quit = true;
        return;
    }

    // Esc 统一在分发表之外处理：任何弹窗（目录浏览 / 选配置 / 保存命名）都靠它关掉，
    // 不依赖各个 handler 是否记得处理，避免出现「弹窗里按 Esc 没反应」。
    if key.code == KeyCode::Esc {
        match &app.mode {
            Mode::Browser(_) | Mode::Picker(_) | Mode::Prompt(_) => {
                app.mode = Mode::Form;
                return;
            }
            Mode::Form => {
                app.should_quit = true;
                return;
            }
            // 运行界面有自己的语义（中断 / 回表单），交给下面的 handler
            Mode::Run(_) => {}
        }
    }

    match &mut app.mode {
        Mode::Form => handle_form_key(app, key),
        Mode::Browser(_) => handle_browser_key(app, key),
        Mode::Picker(_) => handle_picker_key(app, key),
        Mode::Prompt(_) => handle_prompt_key(app, key),
        Mode::Run(_) => handle_run_key(app, key),
    }
}

fn handle_form_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Tab | KeyCode::Down => app.move_field(1),
        KeyCode::BackTab | KeyCode::Up => app.move_field(-1),
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => app.open_prompt(),
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => app.open_picker(),
        // 启动按钮：选中后回车/空格触发
        KeyCode::Enter if app.field == Field::Start => app.start(false),
        KeyCode::Enter if app.field == Field::Preview => app.start(true),
        KeyCode::Enter => {
            if app.field.is_path() {
                app.open_browser();
            } else if app.field == Field::Advanced {
                app.toggle_advanced();
            } else if app.field.is_bool() {
                app.toggle_bool();
            } else if app.field.is_choice() {
                app.cycle(true);
            }
        }
        // 左右键留给「换列」，不再拿去切选项；切值统一用空格（文本字段里空格仍然是输入空格）。
        KeyCode::Left | KeyCode::Right if !app.field.is_text() => {
            app.move_horizontal(key.code == KeyCode::Right);
        }
        // 缓冲区这一行只有数字，空格留着换单位（B/KB/MB/GB）比输入空格有用
        KeyCode::Char(' ') if app.field == Field::BufferSize => app.cycle(true),
        KeyCode::Char(' ') if app.field == Field::Start => app.start(false),
        KeyCode::Char(' ') if app.field == Field::Preview => app.start(true),
        KeyCode::Char(' ') if app.field == Field::Advanced => app.toggle_advanced(),
        KeyCode::Char(' ') if app.field.is_bool() => app.toggle_bool(),
        KeyCode::Char(' ') if app.field.is_choice() => app.cycle(true),
        _ => {
            if app.field.is_text() {
                app.handle_text_key(&key);
            }
        }
    }
}

fn handle_browser_key(app: &mut App, key: KeyEvent) {
    let Mode::Browser(browser) = &mut app.mode else {
        return;
    };
    match key.code {
        KeyCode::Up | KeyCode::BackTab => {
            browser.selected = browser.selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Tab => {
            if browser.selected + 1 < browser.entries.len() {
                browser.selected += 1;
            }
        }
        KeyCode::Home => browser.selected = 0,
        KeyCode::End => browser.selected = browser.entries.len().saturating_sub(1),
        KeyCode::Backspace => {
            // 上一级
            if let Some(parent) = browser.dir.parent() {
                let parent = parent.to_path_buf();
                browser.dir = parent;
                reload_browser(browser);
            }
        }
        KeyCode::Enter => {
            let entry = browser.entries.get(browser.selected).cloned();
            match entry {
                Some(BrowserEntry::Parent) => {
                    if let Some(parent) = browser.dir.parent() {
                        let parent = parent.to_path_buf();
                        browser.dir = parent;
                        reload_browser(browser);
                    }
                }
                Some(BrowserEntry::ChooseHere) => {
                    let chosen = browser.dir.clone();
                    let field = browser.field;
                    if let Some(text) = app.form.text_mut(field) {
                        *text = chosen.to_string_lossy().to_string();
                    }
                    app.mode = Mode::Form;
                }
                Some(BrowserEntry::Dir(path)) => {
                    browser.dir = path;
                    reload_browser(browser);
                }
                None => {}
            }
        }
        KeyCode::Char('h') => {
            // 快速回到主目录
            if let Some(home) = std::env::var_os("HOME") {
                browser.dir = PathBuf::from(home);
                reload_browser(browser);
            }
        }
        _ => {}
    }
}

fn handle_picker_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up => {
            if let Mode::Picker(p) = &mut app.mode {
                p.selected = p.selected.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            if let Mode::Picker(p) = &mut app.mode {
                if p.selected + 1 < p.files.len() {
                    p.selected += 1;
                }
            }
        }
        KeyCode::Enter => {
            let Mode::Picker(p) = &mut app.mode else {
                return;
            };
            let Some(path) = p.files.get(p.selected).cloned() else {
                return;
            };
            match crate::config::load_config(&path) {
                Ok(cfg) => {
                    let mut notice = cfg.legacy_notice();
                    if cfg.dry_run && notice.is_none() {
                        // 界面上已经没有「仅预览」开关了，配置里的 dry_run 不再生效
                        notice = Some(format!(
                            "已加载配置: {}（dry_run 已忽略，预览请点「预览」按钮）",
                            path.display()
                        ));
                    }
                    app.form = FormData::from_config(&cfg);
                    app.mode = Mode::Form;
                    app.status =
                        notice.unwrap_or_else(|| format!("已加载配置: {}", path.display()));
                    app.errors.clear();
                }
                Err(errors) => {
                    app.mode = Mode::Form;
                    app.errors = errors;
                }
            }
        }
        _ => {}
    }
}

fn handle_prompt_key(app: &mut App, key: KeyEvent) {
    let Mode::Prompt(prompt) = &mut app.mode else {
        return;
    };
    match key.code {
        KeyCode::Enter => {
            let mut name = prompt.input.trim().to_string();
            if name.is_empty() {
                prompt.error = Some("文件名不能为空".to_string());
                return;
            }
            if !name.to_lowercase().ends_with(".json") {
                name.push_str(".json");
            }
            let path = app.cwd.join(&name);
            let cfg = app.form.to_config();
            match cfg.save_to_file(&path) {
                Ok(()) => {
                    app.mode = Mode::Form;
                    app.status = format!("已保存配置: {}", path.display());
                }
                Err(e) => prompt.error = Some(format!("保存失败: {e}")),
            }
        }
        KeyCode::Backspace => {
            if prompt.cursor > 0 {
                let idx = byte_index(&prompt.input, prompt.cursor - 1);
                prompt.input.remove(idx);
                prompt.cursor -= 1;
            }
        }
        KeyCode::Left => prompt.cursor = prompt.cursor.saturating_sub(1),
        KeyCode::Right => {
            let len = prompt.input.chars().count();
            prompt.cursor = (prompt.cursor + 1).min(len);
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let idx = byte_index(&prompt.input, prompt.cursor);
            prompt.input.insert(idx, c);
            prompt.cursor += 1;
        }
        _ => {}
    }
}

fn handle_run_key(app: &mut App, key: KeyEvent) {
    let Mode::Run(run) = &mut app.mode else {
        return;
    };
    let total = run.records.len();
    match key.code {
        KeyCode::Esc => {
            if run.summary.is_some() {
                // 已经结束：Esc 返回表单
                if let Some(summary) = run.summary.as_ref() {
                    app.last_exit = summary.exit_code(app.form.fail_empty);
                }
                app.mode = Mode::Form;
            } else {
                // 运行中：请求中断
                run.cancel.store(true, Ordering::Relaxed);
            }
        }
        KeyCode::Char('q') => {
            if run.summary.is_some() {
                if let Some(summary) = run.summary.as_ref() {
                    app.last_exit = summary.exit_code(app.form.fail_empty);
                }
                app.should_quit = true;
            }
        }
        KeyCode::Up => {
            run.follow = false;
            run.scroll = run.scroll.saturating_sub(1);
        }
        KeyCode::Down => {
            if run.scroll + 1 < total {
                run.scroll += 1;
            } else {
                run.follow = true;
            }
        }
        KeyCode::PageUp => {
            run.follow = false;
            run.scroll = run.scroll.saturating_sub(10);
        }
        KeyCode::PageDown => {
            run.scroll = (run.scroll + 10).min(total.saturating_sub(1));
        }
        KeyCode::End => {
            run.follow = true;
            run.scroll = total.saturating_sub(1);
        }
        KeyCode::Home => {
            run.follow = false;
            run.scroll = 0;
        }
        _ => {}
    }
}

/// 运行界面状态文案（标题右侧）。
pub fn run_status_text(run: &RunState) -> String {
    if let Some(summary) = &run.summary {
        if summary.dry_run {
            return format!(
                "预览完成：{} 个文件，{}",
                summary.planned_files,
                format_size(summary.scanned_bytes)
            );
        }
        return format!(
            "完成：成功 {} · 跳过 {} · 覆盖 {} · 失败 {} · {}",
            summary.copied_files,
            summary.skipped_files,
            summary.overrode_files,
            summary.failed_files,
            format_size(summary.copied_bytes)
        );
    }
    if run.cancel.load(Ordering::Relaxed) {
        "正在中断…".to_string()
    } else {
        format!("复制中… {} / {}", run.files_done, run.files_total)
    }
}

/// 记录行颜色（渲染层用）。
pub fn action_color(action: FileAction) -> ratatui::style::Color {
    use ratatui::style::Color;
    match action {
        FileAction::Copied => Color::Green,
        FileAction::Skipped => Color::Yellow,
        FileAction::Overwritten => Color::LightBlue,
        FileAction::Failed => Color::Red,
    }
}

/// 版本号（标题栏用）。
pub fn version() -> &'static str {
    VERSION
}

#[cfg(test)]
mod tests {
    // 测试里逐字段改配置最直观，不必为了消除 lint 写出巨大的结构体字面量
    #![allow(clippy::field_reassign_with_default)]

    use super::*;

    #[test]
    fn form_roundtrip() {
        let mut cfg = Config::default();
        cfg.source_dir = "/tmp/src".into();
        cfg.target_dir = "/tmp/dst".into();
        cfg.extensions = vec!["jpg".into(), "nef".into()];
        cfg.strategy = "tree".into();
        cfg.workers = 4;
        cfg.verify = true;
        let form = FormData::from_config(&cfg);
        let back = form.to_config();
        assert_eq!(back.source_dir, "/tmp/src");
        assert_eq!(back.extensions, vec!["jpg", "nef"]);
        assert_eq!(back.strategy, "tree");
        assert_eq!(back.workers, 4);
        assert!(back.verify);
    }

    #[test]
    fn strategy_cycle_wraps() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.field = Field::Strategy;
        for _ in 0..Strategy::ALL.len() {
            app.cycle(true);
        }
        assert_eq!(Strategy::ALL[app.form.strategy_idx].name(), "date");
    }

    // ---------- 无终端渲染测试：用 TestBackend 真正画一遍界面 ----------

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// 把绘制结果读成纯文本（按行拼接）。
    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn draw(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render::draw(f, app)).unwrap();
        // 双宽字符后面的占位单元会被读成空格，断言前先压掉所有空白
        squash(&screen_text(&terminal))
    }

    fn squash(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn form_screen_shows_basic_fields_when_advanced_collapsed() {
        let app = App::new(None);
        let text = draw(&app, 110, 34);
        for needle in [
            "filecopy",
            "源目录",
            "目标目录",
            "扩展名",
            "排除规则",
            "线程数",
            "高级选项",
            "已收起",
            "操作",
            "[开始]",
            "[预览]",
            "Esc退出",
        ] {
            assert!(
                text.contains(needle),
                "表单缺少 {needle:?}；实际屏幕:\n{text}"
            );
        }
        // 收起时策略/校验这些不该占地方
        for needle in ["分类策略", "日期格式", "覆盖同名", "校验和"] {
            assert!(
                !text.contains(needle),
                "收起状态不该出现 {needle:?}；实际屏幕:\n{text}"
            );
        }
    }

    #[test]
    fn form_screen_shows_all_options_when_advanced_open() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.form.verify = true;
        let text = draw(&app, 110, 40);
        for needle in [
            "策略与开关",
            "分类策略",
            "日期格式",
            "同名文件",
            "跳过",
            "校验和",
            "校验算法",
            "包含隐藏",
            "递归子目录",
            "空扫描算失败",
            "已展开",
        ] {
            assert!(
                text.contains(needle),
                "展开后缺少 {needle:?}；实际屏幕:\n{text}"
            );
        }
    }

    #[test]
    fn form_screen_renders_prefilled_config() {
        let mut cfg = Config::default();
        cfg.source_dir = "/tmp/card".into();
        cfg.target_dir = "/tmp/out".into();
        cfg.strategy = "tree".into();
        cfg.workers = 8;
        let mut app = App::new(Some(cfg));
        app.form.advanced_open = true;
        let text = draw(&app, 120, 34);
        assert!(text.contains("/tmp/card"));
        assert!(text.contains("/tmp/out"));
        assert!(text.contains("tree"));
        assert!(text.contains('8'));
        // 策略说明应跟着当前策略走
        assert!(
            text.contains("保留源目录结构"),
            "应显示 tree 的说明；实际:\n{text}"
        );
    }

    #[test]
    fn tab_moves_between_fields_and_wraps() {
        let mut app = App::new(None);
        assert_eq!(app.field, Field::Source);
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.field, Field::Target);
        handle_key(&mut app, key(KeyCode::BackTab));
        assert_eq!(app.field, Field::Source);
        // 反向过界回绕到最后一行的左列（操作区第一列是「开始」）
        handle_key(&mut app, key(KeyCode::Up));
        assert_eq!(app.field, Field::Start);
        // 同一行往右就是「预览」
        handle_key(&mut app, key(KeyCode::Right));
        assert_eq!(app.field, Field::Preview);
    }

    #[test]
    fn space_toggles_boolean_field() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.field = Field::Verify;
        assert!(!app.form.verify);
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert!(app.form.verify);
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert!(!app.form.verify);
    }

    #[test]
    fn buffer_size_field_is_editable() {
        let mut app = App::new(None);
        // 默认 1 MB，显示成「1 + MB」而不是 1048576
        app.focus(Field::BufferSize);
        assert_eq!(app.form.buffer_size, "1");
        assert_eq!(BUFFER_UNITS[app.form.buffer_unit_idx].0, "MB");
        assert_eq!(app.form.buffer_bytes(), Some(1024 * 1024));
        assert_eq!(app.cursor, 1, "光标应落在末尾");

        // 清空后重新输入
        handle_key(&mut app, key(KeyCode::Backspace));
        assert!(app.form.buffer_size.is_empty());
        for c in "512".chars() {
            handle_key(&mut app, key(KeyCode::Char(c)));
        }
        assert_eq!(app.form.buffer_size, "512");
        assert_eq!(app.cursor, 3);
        // 单位跟着变：空格就停在「缓冲区」这一行上按
        app.focus(Field::BufferSize);
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(
            BUFFER_UNITS[app.form.buffer_unit_idx].0, "GB",
            "MB 下一个是 GB"
        );
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(BUFFER_UNITS[app.form.buffer_unit_idx].0, "B", "GB 绕回 B");
        // 切回 KB：512 KB = 524288 字节
        app.form.buffer_unit_idx = 1;
        assert_eq!(app.form.to_config().buffer_size, 524_288);

        // 空或非数字：留在表单并报错
        app.form.buffer_size = "abc".into();
        let errors = app.validate_form();
        assert!(
            errors.iter().any(|e| e.contains("缓冲区大小")),
            "应提示缓冲区非法: {errors:?}"
        );

        // 超出 4 KB – 64 MB：报错而不是被引擎悄悄夹住
        app.form.buffer_size = "1".into();
        app.form.buffer_unit_idx = 0; // 1 B
        let errors = app.validate_form();
        assert!(
            errors.iter().any(|e| e.contains("4 KB 到 64 MB")),
            "过小应报错: {errors:?}"
        );
        app.form.buffer_size = "0".into();
        assert!(app.validate_form().iter().all(|e| !e.contains("缓冲区")));
    }

    #[test]
    fn buffer_size_splits_into_a_readable_unit() {
        // 1048576 → 1 MB，262144 → 256 KB，3000 → 3000 B
        assert_eq!(split_buffer_size(1024 * 1024), ("1".to_string(), 2));
        assert_eq!(split_buffer_size(256 * 1024), ("256".to_string(), 1));
        assert_eq!(split_buffer_size(3000), ("3000".to_string(), 0));
        assert_eq!(split_buffer_size(0), ("0".to_string(), 0));
    }

    #[test]
    fn dry_run_is_no_longer_a_form_toggle() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.form.verify = true;
        let text = draw(&app, 110, 40);
        assert!(
            !text.contains("仅预览"),
            "「仅预览」开关应已删除；实际:\n{text}"
        );
        // 旧配置里带 dry_run=true 也能加载，只是不再影响表单
        let mut cfg = Config::default();
        cfg.dry_run = true;
        let form = FormData::from_config(&cfg);
        assert!(!form.to_config().dry_run, "表单不再携带 dry_run");
    }

    #[test]
    fn buffer_unit_cycles_with_space_and_shows_bytes() {
        let mut app = App::new(None);
        app.focus(Field::BufferSize);
        assert_eq!(BUFFER_UNITS[app.form.buffer_unit_idx].0, "MB");
        let text = draw(&app, 110, 40);
        assert!(text.contains("[MB]"), "单位应显示为选择器；实际:\n{text}");
        assert!(text.contains("=1048576字节"), "应换算成字节；实际:\n{text}");
        // 空格换到 GB，提示跟着变
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(BUFFER_UNITS[app.form.buffer_unit_idx].0, "GB");
        assert_eq!(app.form.buffer_bytes(), Some(1024 * 1024 * 1024));
        // 超出上限时提示里带范围警告，校验也拦住
        let text = draw(&app, 110, 40);
        assert!(text.contains("64MB之间"), "实际:\n{text}");
        app.field = Field::Start;
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(
            app.errors.iter().any(|e| e.contains("4 KB 到 64 MB")),
            "启动前应拦住过大的缓冲区: {:?}",
            app.errors
        );
    }

    #[test]
    fn date_source_cycles_and_writes_config() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.focus(Field::DateSource);
        assert_eq!(
            app.form.to_config().date_source,
            "created",
            "默认按创建日期"
        );
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(app.form.to_config().date_source, "modified");
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(app.form.to_config().date_source, "created", "两项之间回绕");
    }

    #[test]
    fn date_source_is_only_shown_for_date_strategies() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.form.strategy_idx = 2; // flat
        assert!(!app.form.field_visible(Field::DateSource));
        app.form.strategy_idx = 1; // date_tree
        assert!(app.form.field_visible(Field::DateSource));
        // 收起高级选项时也不该出现
        app.form.advanced_open = false;
        assert!(!app.form.field_visible(Field::DateSource));
    }

    #[test]
    fn example_destination_uses_a_generic_name_and_your_extension() {
        let mut app = App::new(None);
        app.form.extensions = "png".to_string();
        let text = draw(&app, 110, 40);
        assert!(
            text.contains("2026/11/08/文件.png"),
            "示例应写成「文件.png」而不是具体的照片名；实际:\n{text}"
        );
        assert!(
            !text.contains("照片-001"),
            "不该再出现具体的照片名:\n{text}"
        );

        app.form.extensions.clear();
        let text = draw(&app, 110, 40);
        assert!(
            text.contains("2026/11/08/文件") && !text.contains("文件."),
            "没填扩展名时就不带后缀；实际:\n{text}"
        );
    }

    #[test]
    fn date_format_help_is_plain_chinese() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.focus(Field::DateFormat);
        app.form.date_format = "%Y/%m/%d".to_string();
        let text = draw(&app, 110, 40);
        assert!(text.contains("格式说明"), "应有格式说明行；实际:\n{text}");
        assert!(text.contains("年"), "应把 %Y 说成年；实际:\n{text}");
        assert!(text.contains("每个/就是一层新文件夹") || text.contains("新文件夹"));

        // 只输了一半的格式串：说明行照样给提示，不 panic
        app.form.date_format = "%".to_string();
        let text = draw(&app, 110, 40);
        assert!(text.contains("格式还没写完") || text.contains("格式说明"));
    }

    #[test]
    fn buffer_unit_shares_the_row_with_its_value() {
        let mut app = App::new(None);
        app.form.buffer_size = "1".to_string();
        let text = draw(&app, 110, 40);
        // draw() 会把空白压掉，所以这里就是 1[MB] 连在一起
        assert!(
            text.contains("1[MB]"),
            "单位应紧跟在数值后面；实际:\n{text}"
        );
        assert!(text.contains("=1048576字节"));
        app.focus(Field::BufferSize);
        handle_key(&mut app, key(KeyCode::Char(' ')));
        let text = draw(&app, 110, 40);
        assert!(text.contains("1[GB]"), "空格换到 GB；实际:\n{text}");
    }

    #[test]
    fn space_cycles_the_conflict_policy() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.field = Field::Conflict;
        assert_eq!(CONFLICT_POLICIES[app.form.conflict_idx], "跳过");
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(CONFLICT_POLICIES[app.form.conflict_idx], "覆盖");
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(CONFLICT_POLICIES[app.form.conflict_idx], "自动改名");
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(CONFLICT_POLICIES[app.form.conflict_idx], "跳过");
    }

    #[test]
    fn conflict_policy_maps_onto_the_two_config_booleans() {
        let mut app = App::new(None);
        app.form.conflict_idx = 1;
        let cfg = app.form.to_config();
        assert!(cfg.overwrite && !cfg.auto_rename);
        app.form.conflict_idx = 2;
        let cfg = app.form.to_config();
        assert!(!cfg.overwrite && cfg.auto_rename);
        // 从配置反推回三选一
        app.form = FormData::from_config(&cfg);
        assert_eq!(app.form.conflict_idx, 2);
    }

    #[test]
    fn space_cycles_strategy_and_arrows_do_not() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.field = Field::Strategy;
        // 左右键不再改选项
        handle_key(&mut app, key(KeyCode::Right));
        assert_eq!(Strategy::ALL[app.form.strategy_idx].name(), "date");
        // 空格改值
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(Strategy::ALL[app.form.strategy_idx].name(), "date_tree");
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(Strategy::ALL[app.form.strategy_idx].name(), "flat");
    }

    #[test]
    fn left_right_moves_between_paired_fields() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.form.verify = true;
        app.field = Field::Verify;
        handle_key(&mut app, key(KeyCode::Right));
        assert_eq!(app.field, Field::VerifyAlgo);
        // 已经在右列，再往右不动
        handle_key(&mut app, key(KeyCode::Right));
        assert_eq!(app.field, Field::VerifyAlgo);
        handle_key(&mut app, key(KeyCode::Left));
        assert_eq!(app.field, Field::Verify);
        // 单栏字段没有伙伴，左右键不动
        app.field = Field::Strategy;
        handle_key(&mut app, key(KeyCode::Right));
        assert_eq!(app.field, Field::Strategy);
    }

    #[test]
    fn right_column_is_skipped_when_its_dependency_is_off() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.form.verify = false;
        app.field = Field::Verify;
        // 校验关闭时「校验算法」不可见，右移应当原地不动
        handle_key(&mut app, key(KeyCode::Right));
        assert_eq!(app.field, Field::Verify);
        app.form.verify = true;
        handle_key(&mut app, key(KeyCode::Right));
        assert_eq!(app.field, Field::VerifyAlgo);
    }

    #[test]
    fn date_format_only_shows_for_date_strategies() {
        let mut form = FormData::default();
        form.advanced_open = true;
        assert!(form.field_visible(Field::DateFormat));
        form.strategy_idx = Strategy::ALL
            .iter()
            .position(|s| *s == Strategy::Ext)
            .unwrap();
        assert!(!form.field_visible(Field::DateFormat));
        assert!(!form.visible_fields().contains(&Field::DateFormat));
    }

    #[test]
    fn verify_algorithm_only_shows_when_verify_on() {
        let mut form = FormData::default();
        form.advanced_open = true;
        form.verify = false;
        assert!(!form.field_visible(Field::VerifyAlgo));
        form.verify = true;
        assert!(form.field_visible(Field::VerifyAlgo));
    }

    #[test]
    fn space_toggles_advanced_section() {
        let mut app = App::new(None);
        app.field = Field::Advanced;
        assert!(!app.form.advanced_open);
        assert!(!app.form.visible_fields().contains(&Field::Strategy));
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert!(app.form.advanced_open);
        assert!(app.form.visible_fields().contains(&Field::Strategy));
        // 展开后光标应该已经在高级选项区里了
        assert_eq!(app.field, Field::Strategy);
        // 收起要在「高级选项」那一行上按
        app.field = Field::Advanced;
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert!(!app.form.advanced_open);
        assert_eq!(app.field, Field::Advanced, "收起后光标留在开关行上");
    }

    #[test]
    fn expanding_advanced_puts_the_cursor_inside_the_section() {
        let mut app = App::new(None);
        app.field = Field::Advanced;
        assert!(!app.form.advanced_open);
        handle_key(&mut app, key(KeyCode::Char(' ')));
        // 一展开就能直接改策略，不用绕一圈
        assert_eq!(app.field, Field::Strategy);
        // 而且这一行必须真的画在屏幕上（以前光标会落在看不见的行上）
        let text = draw(&app, 110, 40);
        assert!(
            text.contains(">分类策略"),
            "光标所在行应可见且带 > 标记；实际:\n{text}"
        );
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.field, Field::DateFormat);
    }

    #[test]
    fn navigation_follows_the_screen_order() {
        let mut app = App::new(None);
        app.form.advanced_open = false;
        // 收起时按行往下：目录四项 → 线程数 → 缓冲区 → 单位 → 高级选项 → 开始
        let mut seen = vec![app.field];
        for _ in 0..8 {
            handle_key(&mut app, key(KeyCode::Down));
            seen.push(app.field);
        }
        assert_eq!(
            seen,
            vec![
                Field::Source,
                Field::Target,
                Field::Extensions,
                Field::Exclude,
                Field::Workers,
                Field::BufferSize,
                Field::Advanced,
                Field::Start,
                Field::Source,
            ]
        );
    }

    #[test]
    fn collapsed_section_never_puts_the_cursor_on_a_hidden_row() {
        // 按日期策略 + 开校验，但高级选项收起：这两行不在屏幕上
        let mut app = App::new(None);
        app.form.advanced_open = false;
        app.form.strategy_idx = 1; // date_tree
        app.form.verify = true;
        for visible in app.form.visible_fields() {
            assert!(
                !matches!(visible, Field::DateFormat | Field::VerifyAlgo),
                "收起时不该把 {visible:?} 当作可停留的行"
            );
        }
    }

    #[test]
    fn collapsing_advanced_moves_focus_out_of_hidden_field() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.field = Field::FailEmpty;
        // 先回到「高级选项」行再收起
        app.field = Field::Advanced;
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert!(!app.form.advanced_open);
        assert!(app.form.field_visible(app.field));
    }

    /// 造一个能跑通的最小配置（源目录里一个文件）。
    fn small_config(tmp: &std::path::Path) -> Config {
        let src = tmp.join("card");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.jpg"), b"1234").unwrap();
        let mut cfg = Config::default();
        cfg.source_dir = src.to_string_lossy().to_string();
        cfg.target_dir = tmp.join("out").to_string_lossy().to_string();
        cfg.strategy = "flat".into();
        cfg
    }

    /// 等后台线程跑完。
    fn wait_for_summary(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            app.pump_run();
            let done = matches!(&app.mode, Mode::Run(run) if run.summary.is_some());
            if done || std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn form_has_start_and_preview_buttons() {
        let app = App::new(None);
        let text = draw(&app, 110, 34);
        for needle in ["操作", "[开始]", "[预览]"] {
            assert!(text.contains(needle), "缺少 {needle:?}；实际:\n{text}");
        }
    }

    #[test]
    fn space_on_start_button_runs_the_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = small_config(tmp.path());
        let mut app = App::new(Some(cfg));

        // 用 ↑ 从「高级选项」走到启动按钮
        app.field = Field::Start;
        handle_key(&mut app, key(KeyCode::Char(' ')));
        assert!(matches!(app.mode, Mode::Run(_)), "空格应启动运行");

        wait_for_summary(&mut app);
        let Mode::Run(run) = &app.mode else {
            panic!("还在表单");
        };
        let summary = run.summary.as_ref().expect("引擎应在 5 秒内结束");
        assert_eq!(summary.copied_files, 1);
        assert!(!summary.dry_run);
        assert!(tmp.path().join("out/a.jpg").exists());
    }

    #[test]
    fn enter_on_preview_button_runs_dry_run_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = small_config(tmp.path());
        let mut app = App::new(Some(cfg));

        app.field = Field::Preview;
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Run(_)), "回车应启动预览");

        wait_for_summary(&mut app);
        let Mode::Run(run) = &app.mode else {
            panic!("还在表单");
        };
        let summary = run.summary.as_ref().expect("引擎应在 5 秒内结束");
        assert!(summary.dry_run, "预览按钮必须走 dry-run");
        assert_eq!(summary.copied_files, 0);
        assert!(!tmp.path().join("out").exists(), "预览不该写盘");

        let text = draw(&app, 110, 34);
        assert!(text.contains("预览完成"), "标题应注明预览；实际:\n{text}");
    }

    #[test]
    fn advanced_content_renders_below_its_toggle_row() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.form.verify = true;
        let text = draw(&app, 110, 40);
        let toggle = text.find("高级选项").expect("应有高级选项行");
        let strategy = text.find("分类策略").expect("展开后应有分类策略");
        assert!(
            toggle < strategy,
            "展开的内容应出现在「高级选项」下方；实际:\n{text}"
        );
    }

    #[test]
    fn start_button_comes_before_preview_when_tabbing_down() {
        let mut app = App::new(None);
        app.field = Field::Advanced;
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.field, Field::Start, "↓ 应该先到左边的「开始」");
        // 两个按钮在同一行，靠右键过去
        handle_key(&mut app, key(KeyCode::Right));
        assert_eq!(app.field, Field::Preview);
        // 左右键在并排的两个按钮之间移动
        handle_key(&mut app, key(KeyCode::Left));
        assert_eq!(app.field, Field::Start);
    }

    #[test]
    fn moving_down_keeps_the_current_column() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.form.verify = true;
        // 左列往下仍是左列
        app.field = Field::Verify;
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.field, Field::IncludeHidden);
        // 右列往下仍是右列（这是用户报的那个问题）
        app.field = Field::VerifyAlgo;
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.field, Field::Recursive);
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.field, Field::FailEmpty);
        // 右列往上同理（从「空扫描算失败」这个单栏行回到右列）
        app.field = Field::Recursive;
        handle_key(&mut app, key(KeyCode::Up));
        assert_eq!(app.field, Field::VerifyAlgo);
        // 缓冲区（数字 + 单位同一行）往下就是「高级选项」
        app.field = Field::BufferSize;
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.field, Field::Advanced);
    }

    #[test]
    fn action_buttons_share_one_line() {
        let mut app = App::new(None);
        app.field = Field::Start;
        let text = draw(&app, 110, 34);
        let start = text.find("[开始]").expect("应有开始按钮");
        let preview = text.find("[预览]").expect("应有预览按钮");
        // 同一行：两者之间不应出现换行留下的行框（顶栏/底栏字符）
        let between = &text[start..preview];
        assert!(
            !between.contains('└') && !between.contains('┘') && between.len() < 20,
            "两个按钮应在同一行；夹在中间的是 {between:?}"
        );
    }

    #[test]
    fn footer_hint_follows_the_focused_field() {
        // draw() 会压掉空白，这里比较的是去空格后的片段
        let cases = [
            (Field::Source, "回车选目录"),
            (Field::Advanced, "空格展开/收起"),
            (Field::Strategy, "空格改值"),
            (Field::Start, "回车/空格启动"),
            (Field::Preview, "回车/空格启动"),
            (Field::Workers, "直接输入"),
        ];
        for (field, hint) in cases {
            let mut app = App::new(None);
            app.form.advanced_open = true;
            app.field = field;
            let text = draw(&app, 140, 40);
            assert!(
                text.contains(hint),
                "字段 {field:?} 的页脚缺少 {hint:?}；实际:\n{text}"
            );
            assert!(!text.contains("F5"), "页脚不该再提 F5；实际:\n{text}");
        }
    }

    #[test]
    fn footer_shows_column_hint_only_for_paired_fields() {
        let mut app = App::new(None);
        app.form.advanced_open = true;
        app.field = Field::Verify;
        assert!(draw(&app, 140, 40).contains("←→换列"));
        app.field = Field::Source;
        assert!(!draw(&app, 140, 40).contains("←→换列"));
    }

    #[test]
    fn typing_edits_text_field_at_cursor() {
        let mut app = App::new(None);
        // 默认光标在末尾（空字段）
        for c in "/tmp/card".chars() {
            handle_key(&mut app, key(KeyCode::Char(c)));
        }
        assert_eq!(app.form.source, "/tmp/card");
        handle_key(&mut app, key(KeyCode::Backspace));
        assert_eq!(app.form.source, "/tmp/car");
        // 左移两次后插入，应插在中间
        handle_key(&mut app, key(KeyCode::Left));
        handle_key(&mut app, key(KeyCode::Left));
        handle_key(&mut app, key(KeyCode::Char('X')));
        assert_eq!(app.form.source, "/tmp/cXar");
    }

    #[test]
    fn starting_with_an_invalid_config_stays_on_form_and_shows_errors() {
        let mut app = App::new(None);
        app.field = Field::Start;
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Form));
        assert!(!app.errors.is_empty(), "空配置应报错");
        let text = draw(&app, 110, 34);
        assert!(text.contains('✗'), "错误应以 ✗ 显示；实际:\n{text}");
    }

    #[test]
    fn start_button_with_a_valid_config_enters_run_mode_and_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("card");
        let dst = tmp.path().join("out");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.jpg"), b"1234").unwrap();

        let mut cfg = Config::default();
        cfg.source_dir = src.to_string_lossy().to_string();
        cfg.target_dir = dst.to_string_lossy().to_string();
        cfg.strategy = "flat".into();
        cfg.dry_run = false;

        let mut app = App::new(Some(cfg));
        app.field = Field::Start;
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(
            matches!(app.mode, Mode::Run(_)),
            "在「开始」上回车之后应进入运行界面"
        );

        // 等后台线程跑完（引擎很小，几十毫秒足够）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            app.pump_run();
            let done = matches!(&app.mode, Mode::Run(run) if run.summary.is_some());
            if done || std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let Mode::Run(run) = &app.mode else {
            panic!("还在表单");
        };
        let summary = run.summary.as_ref().expect("引擎应在 5 秒内结束");
        assert_eq!(summary.copied_files, 1);
        assert_eq!(run.records.len(), 1);

        let text = draw(&app, 110, 34);
        assert!(text.contains("复制记录"));
        assert!(text.contains("a.jpg"));
        assert!(text.contains("完成"), "标题应显示完成状态；实际:\n{text}");
        assert!(std::fs::read(dst.join("a.jpg")).unwrap() == b"1234");

        // Esc 回到表单并带回退出码
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Form));
        assert_eq!(app.last_exit, 0);
    }

    #[test]
    fn run_screen_renders_records_progress_and_logs() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(None);
        app.mode = Mode::Run(RunState {
            rx,
            handle: None,
            cancel: Arc::new(AtomicBool::new(false)),
            records: Vec::new(),
            logs: Vec::new(),
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            summary: None,
            scroll: 0,
            follow: true,
        });

        tx.send(EngineEvent::Log("扫描结果: 找到 2 个文件".into()))
            .unwrap();
        tx.send(EngineEvent::Progress {
            files_done: 1,
            files_total: 2,
            bytes_done: 4,
            bytes_total: 8,
        })
        .unwrap();
        tx.send(EngineEvent::Record(CopyRecord {
            src: PathBuf::from("/tmp/card/DCIM/IMG_0001.JPG"),
            dst: PathBuf::from("/tmp/out/IMG_0001.JPG"),
            action: FileAction::Copied,
            detail: "sha256 校验通过".into(),
        }))
        .unwrap();
        tx.send(EngineEvent::Record(CopyRecord {
            src: PathBuf::from("/tmp/card/DCIM/IMG_0002.JPG"),
            dst: PathBuf::from("/tmp/out/IMG_0002.JPG"),
            action: FileAction::Skipped,
            detail: "目标已存在".into(),
        }))
        .unwrap();
        app.pump_run();

        let text = draw(&app, 120, 34);
        assert!(text.contains("复制记录"), "实际:\n{text}");
        assert!(text.contains("IMG_0001.JPG"));
        assert!(text.contains("IMG_0002.JPG"));
        assert!(text.contains("复制"), "应显示动作列");
        assert!(text.contains("跳过"));
        assert!(text.contains("扫描结果"), "日志区应显示最新日志");
        assert!(text.contains("1/2"), "文件进度条应显示 1/2；实际:\n{text}");
    }

    #[test]
    fn finished_run_screen_marks_dry_run_in_title() {
        let (_tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
        let mut app = App::new(None);
        let mut summary = Summary::default();
        summary.dry_run = true;
        summary.planned_files = 7;
        summary.scanned_bytes = 2048;
        app.mode = Mode::Run(RunState {
            rx,
            handle: None,
            cancel: Arc::new(AtomicBool::new(false)),
            records: Vec::new(),
            logs: Vec::new(),
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            summary: Some(summary),
            scroll: 0,
            follow: true,
        });
        let text = draw(&app, 110, 30);
        assert!(
            text.contains("预览完成"),
            "dry-run 标题应注明预览；实际:\n{text}"
        );
    }

    #[test]
    fn esc_while_running_requests_cancellation_instead_of_quitting() {
        let (_tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = App::new(None);
        app.mode = Mode::Run(RunState {
            rx,
            handle: None,
            cancel: cancel.clone(),
            records: Vec::new(),
            logs: Vec::new(),
            files_done: 0,
            files_total: 3,
            bytes_done: 0,
            bytes_total: 9,
            summary: None,
            scroll: 0,
            follow: true,
        });
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(cancel.load(Ordering::Relaxed), "Esc 应请求中断");
        assert!(!app.should_quit, "运行中 Esc 不应直接退出程序");
        assert!(matches!(app.mode, Mode::Run(_)));
    }

    #[test]
    fn browser_lists_only_directories_and_picks_current_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("file.txt"), b"x").unwrap();
        std::fs::create_dir_all(tmp.path().join(".hidden_dir")).unwrap();

        let mut app = App::new(None);
        app.cwd = tmp.path().to_path_buf();
        app.field = Field::Source;
        handle_key(&mut app, key(KeyCode::Enter));
        let Mode::Browser(browser) = &app.mode else {
            panic!("Enter 应打开目录浏览器");
        };
        let labels: Vec<String> = browser.entries.iter().map(|e| e.label()).collect();
        assert!(labels.iter().any(|l| l.contains("sub")));
        assert!(
            !labels.iter().any(|l| l.contains("file.txt")),
            "文件不应列出"
        );
        assert!(
            !labels.iter().any(|l| l.contains(".hidden_dir")),
            "隐藏目录不应列出"
        );

        // 选中「使用当前目录」并确认
        let mode = std::mem::replace(&mut app.mode, Mode::Form);
        let heading = match mode {
            Mode::Browser(b) => b,
            _ => unreachable!(),
        };
        let mut browser = heading;
        browser.selected = 1; // [ 使用当前目录 ]
        app.mode = Mode::Browser(browser);
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Form));
        assert_eq!(app.form.source, tmp.path().to_string_lossy().to_string());
    }

    #[test]
    fn esc_closes_every_modal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::new(None);
        app.cwd = tmp.path().to_path_buf();

        // 目录浏览器
        app.field = Field::Source;
        app.open_browser();
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Form), "Esc 应关闭目录浏览器");

        // 保存对话框
        app.open_prompt();
        assert!(matches!(app.mode, Mode::Prompt(_)));
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Form), "Esc 应关闭保存对话框");

        // 配置选择
        app.form.source = tmp.path().to_string_lossy().to_string();
        app.form.target = tmp.path().join("out").to_string_lossy().to_string();
        app.form
            .to_config()
            .save_to_file(&tmp.path().join("saved.json"))
            .unwrap();
        app.open_picker();
        assert!(matches!(app.mode, Mode::Picker(_)));
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Form), "Esc 应关闭配置选择");

        assert!(!app.should_quit, "关弹窗的 Esc 都不应退出程序");
    }

    #[test]
    fn key_release_events_are_ignored() {
        let mut app = App::new(None);
        let mut ev = key(KeyCode::Esc);
        ev.kind = KeyEventKind::Release;
        handle_key(&mut app, ev);
        assert!(!app.should_quit, "松开事件不应触发退出");
        assert!(matches!(app.mode, Mode::Form));
    }

    #[test]
    fn renders_every_mode_at_many_terminal_sizes() {
        use ratatui::backend::TestBackend;

        let tmp = tempfile::tempdir().unwrap();
        let mut apps: Vec<(&str, App)> = Vec::new();

        let form = App::new(None);
        apps.push(("form", form));

        let mut browser_app = App::new(None);
        browser_app.cwd = tmp.path().to_path_buf();
        browser_app.field = Field::Source;
        browser_app.open_browser();
        apps.push(("browser", browser_app));

        let mut picker_app = App::new(None);
        picker_app.cwd = tmp.path().to_path_buf();
        picker_app.open_prompt();
        apps.push(("prompt", picker_app));

        let mut picker_app = App::new(None);
        picker_app.status = "没有配置".to_string();
        apps.push(("form-with-status", picker_app));

        let (_tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
        let mut run_app = App::new(None);
        run_app.mode = Mode::Run(RunState {
            rx,
            handle: None,
            cancel: Arc::new(AtomicBool::new(false)),
            records: vec![CopyRecord {
                src: PathBuf::from("/nonexistent/IMG_0001.JPG"),
                dst: PathBuf::from("/nonexistent/out/IMG_0001.JPG"),
                action: FileAction::Copied,
                detail: "sha256 校验通过".to_string(),
            }],
            logs: vec!["扫描结果：找到 1 个文件".to_string()],
            files_done: 1,
            files_total: 1,
            bytes_done: 10,
            bytes_total: 10,
            summary: Some(Summary::default()),
            scroll: 0,
            follow: true,
        });
        apps.push(("run", run_app));

        let sizes = [
            (1, 1),
            (10, 3),
            (20, 6),
            (33, 9),
            (40, 10),
            (62, 14),
            (80, 24),
            (160, 50),
        ];
        for (name, app) in &apps {
            for (w, h) in sizes {
                let mut terminal = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
                terminal
                    .draw(|f| render::draw(f, app))
                    .unwrap_or_else(|e| panic!("{name} 在 {w}x{h} 下渲染失败: {e}"));
            }
        }
    }

    #[test]
    fn random_key_sequences_keep_rendering_without_panic() {
        use ratatui::backend::TestBackend;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("a.jpg"), b"1234").unwrap();

        let mut app = App::new(None);
        app.cwd = tmp.path().to_path_buf();

        // 不包含 F5：本测试只验证「任何按键序列都不会 panic 且画面画得出来」，
        // 真跑复制由专门的测试覆盖。
        let codes = [
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Backspace,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Char(' '),
            KeyCode::Char('a'),
            KeyCode::Char('h'),
            KeyCode::Char('s'),
            KeyCode::Char('o'),
            KeyCode::Char('q'),
            KeyCode::Char('中'),
        ];

        let mut seed = 0x5DEECE66Du64;
        for _ in 0..1500 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let idx = (seed >> 33) as usize % codes.len();
            let mods = if seed.is_multiple_of(5) {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            };
            let mut ev = key(codes[idx]);
            ev.modifiers = mods;
            handle_key(&mut app, ev);
            // 允许「退出」，但测试要继续折腾别的模式
            app.should_quit = false;

            let mut terminal = ratatui::Terminal::new(TestBackend::new(96, 28)).unwrap();
            terminal.draw(|f| render::draw(f, &app)).unwrap();
        }
    }

    #[test]
    fn esc_closes_directory_browser_and_returns_to_form() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::new(None);
        app.cwd = tmp.path().to_path_buf();
        app.field = Field::Source;
        app.open_browser();
        assert!(matches!(app.mode, Mode::Browser(_)));
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(
            matches!(app.mode, Mode::Form),
            "Esc 应关闭目录浏览器回到表单"
        );
        assert!(!app.should_quit, "关浏览器的那次 Esc 不应退出程序");
    }

    #[test]
    fn prompt_save_appends_json_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::new(None);
        app.cwd = tmp.path().to_path_buf();
        app.open_prompt();
        let Mode::Prompt(p) = &mut app.mode else {
            panic!("应打开保存对话框");
        };
        p.input = "my-config".to_string();
        p.cursor = p.input.chars().count();
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Form));
        assert!(tmp.path().join("my-config.json").exists(), "应自动补 .json");
        assert!(app.status.contains("已保存"));
    }
}
