//! 引擎：扫描 → 计划 → 复制（可选并行）→ 汇总。
//!
//! 界面无关：所有结果通过 `EngineEvent` 频道抛给调用方（TUI 收下来画表，
//! 无界面模式直接打印）。计数器只在主线程一侧累加 —— 界面与进度条都不
//! 线程安全，让工作线程直接上报只会让记录顺序和汇总数字互相打架。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::Config;
use crate::copy::{self, CopyError, CopyOutcome};
use crate::model::{display_path, format_size, CopyRecord, FileAction, Strategy};
use crate::plan::{Plan, PlanItem};
use crate::scan::{scan_sources, FileFilter};

/// 引擎抛给界面的消息。
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// 一行日志（已带前缀信息，界面只负责换行/着色）。
    Log(String),
    /// 一条文件级结果，界面塞进可滚动记录表。
    Record(CopyRecord),
    /// 进度：已完成文件数 / 总数 / 已写字节 / 应写字节。
    Progress {
        files_done: usize,
        files_total: usize,
        bytes_done: u64,
        bytes_total: u64,
    },
    /// 结束。
    Finished(Summary),
}

/// 一次运行的汇总。
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub scanned_files: usize,
    pub scanned_bytes: u64,
    pub copied_files: usize,
    pub copied_bytes: u64,
    pub skipped_files: usize,
    pub overrode_files: usize,
    pub failed_files: usize,
    pub verified_files: usize,
    pub empty_scan: bool,
    pub dry_run: bool,
    pub cancelled: bool,
    /// 计划里真正打算写入的字节（进度条分母）。
    pub pending_bytes: u64,
    /// 计划条数。
    pub planned_files: usize,
}

impl Summary {
    /// 进程退出码：0 = 无失败；1 = 有失败，或空扫描且开了 fail_empty。
    ///
    /// 空扫描默认算成功 —— 相机卡插着但还没拍，不该让脚本报错。
    pub fn exit_code(&self, fail_empty: bool) -> i32 {
        if self.cancelled {
            return crate::model::EXIT_SIGINT;
        }
        if self.failed_files > 0 {
            return crate::model::EXIT_ERROR;
        }
        if self.empty_scan && fail_empty {
            return crate::model::EXIT_ERROR;
        }
        0
    }

    /// 成功数 = 新复制 + 覆盖。
    pub fn success_files(&self) -> usize {
        self.copied_files + self.overrode_files
    }
}

/// 工作线程回传的结果队列：(计划下标, 复制结果)。
type CopyResults = Arc<Mutex<Vec<(usize, Result<CopyOutcome, CopyError>)>>>;

/// 引擎：配置 + 中断标志 + 事件出口。
pub struct Engine {
    pub config: Config,
    pub cancel: Arc<AtomicBool>,
    tx: Sender<EngineEvent>,
}

impl Engine {
    pub fn new(config: Config, cancel: Arc<AtomicBool>) -> (Self, Receiver<EngineEvent>) {
        let (tx, rx) = mpsc::channel();
        (Self { config, cancel, tx }, rx)
    }

    fn log(&self, msg: impl Into<String>) {
        let _ = self.tx.send(EngineEvent::Log(msg.into()));
    }

    /// 跑完整流程；返回值同时也会作为 `EngineEvent::Finished` 发出。
    pub fn run(&mut self) -> Summary {
        let mut summary = Summary::default();
        let strategy = self.config.strategy();
        let src_root = self.config.src_path();
        let dst_root = self.config.dst_path();

        let filter = FileFilter::new(
            self.config.extensions.clone(),
            self.config.exclude.clone(),
            self.config.include_hidden,
            self.config.recursive,
            Some(src_root.clone()),
        );
        let outcome = scan_sources(&src_root, &filter);
        if !outcome.unreadable_dirs.is_empty() {
            self.log(format!(
                "权限错误: {} 个目录无法读取，其中的文件不会出现在计划里",
                outcome.unreadable_dirs.len()
            ));
            for d in outcome.unreadable_dirs.iter().take(5) {
                self.log(format!("  • {d}"));
            }
            if outcome.unreadable_dirs.len() > 5 {
                self.log(format!("  … 还有 {} 个", outcome.unreadable_dirs.len() - 5));
            }
        }

        // 每个文件只 stat 一次：预览、进度分母、汇总都要用
        let mut sizes: HashMap<PathBuf, u64> = HashMap::new();
        let mut total_bytes: u64 = 0;
        for f in &outcome.files {
            if let Ok(meta) = std::fs::metadata(f) {
                sizes.insert(f.clone(), meta.len());
                total_bytes += meta.len();
            }
        }
        summary.scanned_files = outcome.files.len();
        summary.scanned_bytes = total_bytes;

        if outcome.files.is_empty() {
            summary.empty_scan = true;
            self.log("警告: 没有需要复制的文件");
            if self.config.fail_empty {
                self.log("已启用 fail_empty：本次以非零退出码结束。");
            }
            self.finish(&summary);
            return summary;
        }

        self.log(format!(
            "扫描结果: 找到 {} 个文件，总大小 {}",
            outcome.files.len(),
            format_size(total_bytes)
        ));
        self.log(format!(
            "分类策略: {}",
            strategy.describe(
                &self.config.date_format,
                crate::model::DateSource::from_name(&self.config.date_source).unwrap_or_default()
            )
        ));
        if self.config.workers > 1 {
            self.log(format!("并行复制: {} 个线程", self.config.workers));
        }
        if self.config.verify {
            self.log(format!(
                "校验和验收: {}（复制后比对）",
                self.config.verify_algorithm
            ));
        }

        let mut warnings = Vec::new();
        let raw = Plan::raw(
            &outcome.files,
            strategy,
            &src_root,
            &dst_root,
            &self.config.date_format,
            crate::model::DateSource::from_name(&self.config.date_source).unwrap_or_default(),
            &mut warnings,
        );
        for w in &warnings {
            self.log(w.clone());
        }
        let plan = Plan::build(raw, self.config.auto_rename, self.config.overwrite, true);
        self.report_collisions(&plan, &src_root, &dst_root);

        let pending = plan.pending_bytes(self.config.overwrite, |p| {
            sizes.get(p).copied().unwrap_or(0)
        });
        summary.pending_bytes = pending;
        summary.planned_files = plan.items.len();

        if self.config.dry_run {
            summary.dry_run = true;
            // 预览也把进度推一遍：否则界面上的文件数/数据量一直停在 0，看起来像卡住。
            let total = plan.items.len();
            let mut bytes_done = 0u64;
            let _ = self.tx.send(EngineEvent::Progress {
                files_done: 0,
                files_total: total,
                bytes_done,
                bytes_total: pending,
            });
            for (index, item) in plan.items.iter().enumerate() {
                if plan.will_write(item, self.config.overwrite) {
                    bytes_done += sizes.get(&item.src).copied().unwrap_or(0);
                }
                let _ = self.tx.send(EngineEvent::Progress {
                    files_done: index + 1,
                    files_total: total,
                    bytes_done,
                    bytes_total: pending,
                });
            }
            self.log("DRY-RUN：仅预览，不写入任何文件");
            self.log(format!("源根目录: {}", src_root.display()));
            self.log(format!("目标根目录: {}", dst_root.display()));
            self.log(format!(
                "复制计划预览（前 {} 条，共 {} 条）",
                plan.items.len().min(12),
                plan.items.len()
            ));
            for item in plan.items.iter().take(12) {
                let size = sizes
                    .get(&item.src)
                    .map(|b| format_size(*b))
                    .unwrap_or_else(|| "?".to_string());
                self.log(format!(
                    "{} → {}  [{}]",
                    display_path(&item.src, &src_root),
                    display_path(&item.dst, &dst_root),
                    size
                ));
            }
            if plan.items.len() > 12 {
                self.log(format!(
                    "… 还有 {} 个文件未列出（共 {}）",
                    plan.items.len() - 12,
                    format_size(total_bytes)
                ));
            }
            self.finish(&summary);
            return summary;
        }

        self.execute(&plan, &sizes, pending, &mut summary);
        self.log_summary(&summary);
        self.finish(&summary);
        summary
    }

    fn finish(&self, summary: &Summary) {
        let _ = self.tx.send(EngineEvent::Finished(summary.clone()));
    }

    fn log_summary(&self, summary: &Summary) {
        self.log(format!(
            "成功 {} 个（覆盖 {}），跳过 {}，失败 {}",
            summary.copied_files,
            summary.overrode_files,
            summary.skipped_files,
            summary.failed_files
        ));
        if summary.verified_files > 0 {
            self.log(format!("校验通过 {} 个", summary.verified_files));
        }
        self.log(format!("数据量 {}", format_size(summary.copied_bytes)));
        if summary.cancelled {
            self.log("已中断：未完成的文件不会被替换，目标目录中原有文件保持完好。");
        }
    }

    fn report_collisions(&self, plan: &Plan, src_root: &Path, dst_root: &Path) {
        if plan.collisions.is_empty() {
            return;
        }
        let total: usize = plan.collisions.iter().map(|c| c.sources.len()).sum();
        if self.config.auto_rename {
            self.log(format!(
                "检测到 {} 组目标路径重名（涉及 {total} 个文件）；已开启自动改名，将追加 _1/_2 后缀全部保留。",
                plan.collisions.len()
            ));
        } else {
            self.log(format!(
                "目标路径冲突: {} 个目标路径被多个源文件占用（共 {total} 个文件）。\
                 它们会互相跳过/覆盖（取决于是否开启覆盖），可能丢失数据。\
                 建议开启自动改名，或改用 tree / date_tree 策略。",
                plan.collisions.len()
            ));
        }
        for (dst, sources) in plan.collision_rows(src_root, dst_root) {
            self.log(format!("  {dst} ← {}", sources.replace('\n', " | ")));
        }
    }

    /// 执行计划：workers 个线程取任务，结果回主线程统一计数上报。
    fn execute(
        &self,
        plan: &Plan,
        sizes: &HashMap<PathBuf, u64>,
        pending_bytes: u64,
        summary: &mut Summary,
    ) {
        let total = plan.items.len().max(1);
        let workers = self.config.workers.clamp(1, crate::model::MAX_WORKERS);

        let items = Arc::new(plan.items.clone());
        let duplicates = Arc::new(plan.duplicates.clone());
        let config = Arc::new(self.config.clone());
        let cancel = self.cancel.clone();
        let sizes = Arc::new(sizes.clone());
        let src_root = self.config.src_path();
        let next = Arc::new(AtomicUsize::new(0));
        let results: CopyResults = Arc::new(Mutex::new(Vec::new()));

        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let items = items.clone();
            let duplicates = duplicates.clone();
            let config = config.clone();
            let cancel = cancel.clone();
            let src_root = src_root.clone();
            let next = next.clone();
            let results = results.clone();
            handles.push(std::thread::spawn(move || loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= items.len() {
                    break;
                }
                let item = &items[idx];
                let dup = duplicates.get(&item.src).cloned();
                let cancelled_flag = cancel.clone();
                let cancelled = move || cancelled_flag.load(Ordering::Relaxed);
                let outcome = copy::copy_one(
                    &config,
                    &item.src,
                    &item.dst,
                    dup.as_deref(),
                    &src_root,
                    &cancelled,
                );
                if let Ok(mut guard) = results.lock() {
                    guard.push((idx, outcome));
                } else {
                    break;
                }
            }));
        }

        let mut files_done = 0usize;
        let mut bytes_done = 0u64;
        let mut reported: HashSet<usize> = HashSet::with_capacity(plan.items.len());

        // 主线程边收边报：界面能即时滚动，进度条也走得平滑
        loop {
            let alive = handles.iter().any(|h| !h.is_finished());
            let batch = {
                let mut guard = results.lock().unwrap();
                std::mem::take(&mut *guard)
            };
            if batch.is_empty() {
                if !alive {
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
                continue;
            }
            for (idx, outcome) in batch {
                if reported.insert(idx) {
                    Self::report_one(
                        self,
                        sizes.get(&items[idx].src).copied().unwrap_or(0),
                        items[idx].src.clone(),
                        items[idx].dst.clone(),
                        outcome,
                        &mut files_done,
                        &mut bytes_done,
                        summary,
                    );
                    let _ = self.tx.send(EngineEvent::Progress {
                        files_done,
                        files_total: total,
                        bytes_done,
                        bytes_total: pending_bytes,
                    });
                }
            }
            if !alive && reported.len() >= plan.items.len() {
                break;
            }
        }

        for handle in handles {
            let _ = handle.join();
        }

        // 收尾：join 之后可能还有结果没被取走
        let leftover = {
            let mut guard = results.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        for (idx, outcome) in leftover {
            if reported.insert(idx) {
                Self::report_one(
                    self,
                    sizes.get(&items[idx].src).copied().unwrap_or(0),
                    items[idx].src.clone(),
                    items[idx].dst.clone(),
                    outcome,
                    &mut files_done,
                    &mut bytes_done,
                    summary,
                );
            }
        }

        if self.cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
        }

        // 中断时没被处理的项如实报失败：汇总不能显示「全部成功」
        let mut missing = 0usize;
        for (idx, item) in plan.items.iter().enumerate() {
            if reported.contains(&idx) {
                continue;
            }
            missing += 1;
            summary.failed_files += 1;
            let _ = self.tx.send(EngineEvent::Record(CopyRecord {
                src: item.src.clone(),
                dst: item.dst.clone(),
                action: FileAction::Failed,
                detail: "已中断，该文件未被处理".to_string(),
            }));
        }
        if missing > 0 {
            files_done += missing;
        }

        let _ = self.tx.send(EngineEvent::Progress {
            files_done,
            files_total: total,
            bytes_done,
            bytes_total: pending_bytes,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn report_one(
        &self,
        size: u64,
        src: PathBuf,
        dst: PathBuf,
        outcome: Result<CopyOutcome, CopyError>,
        files_done: &mut usize,
        bytes_done: &mut u64,
        summary: &mut Summary,
    ) {
        let (action, note) = match outcome {
            Ok(out) => (out.action, out.note),
            Err(CopyError::Interrupted) => {
                (FileAction::Failed, "已中断，该文件未被处理".to_string())
            }
            Err(CopyError::Failed(msg)) => (FileAction::Failed, msg),
        };
        match action {
            FileAction::Copied => {
                summary.copied_files += 1;
                summary.copied_bytes += size;
            }
            FileAction::Overwritten => {
                summary.overrode_files += 1;
                summary.copied_bytes += size;
            }
            FileAction::Skipped => summary.skipped_files += 1,
            FileAction::Failed => summary.failed_files += 1,
        }
        if action.is_written() && note.contains("校验通过") {
            summary.verified_files += 1;
        }
        *files_done += 1;
        if action.is_written() {
            *bytes_done += size;
        }

        let _ = self.tx.send(EngineEvent::Record(CopyRecord {
            src,
            dst,
            action,
            detail: note,
        }));
    }
}

/// 目标目录是否可写（提前检查，避免跑完才发现没权限）。
pub fn ensure_target_writable(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).is_ok() && copy::is_writable_dir(dir)
}

/// 让 `Strategy` 的警告也能在无界面模式下拿到（测试辅助）。
pub fn strategy_of(config: &mut Config) -> Strategy {
    config.strategy()
}

/// 计划条数与重复项数量的只读视图（测试辅助）。
pub fn plan_counts(plan: &Plan) -> (usize, usize) {
    (plan.items.len(), plan.duplicates.len())
}

/// 计划项的只读视图（测试辅助）。
pub fn plan_items(plan: &Plan) -> &[PlanItem] {
    &plan.items
}
