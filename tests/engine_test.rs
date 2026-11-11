//! 端到端测试：拿真实的临时目录跑整个引擎，验证与 Python 版一致的行为。
//!
//! 这里刻意不碰界面层（TUI 需要终端，不适合在 CI 里跑），只测「配置 → 扫描 →
//! 计划 → 复制 → 汇总」这条链路，以及退出码约定。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use filecopy_rs::config::Config;
use filecopy_rs::engine::{Engine, EngineEvent, Summary};
use filecopy_rs::model::{FileAction, EXIT_ERROR, EXIT_SIGINT};

// ======================== 测试辅助 ========================

#[allow(clippy::field_reassign_with_default)] // 测试里逐字段改配置最直观
fn base_config(src: &Path, dst: &Path, strategy: &str) -> Config {
    let mut cfg = Config::default();
    cfg.source_dir = src.to_string_lossy().to_string();
    cfg.target_dir = dst.to_string_lossy().to_string();
    cfg.strategy = strategy.to_string();
    cfg.normalize_paths();
    cfg
}

fn write_file(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("建目录");
    }
    std::fs::write(path, content).expect("写文件");
}

/// 同步跑一遍引擎，把事件也收下来（频道是 unbounded，不会堵住）。
fn run_engine(cfg: Config) -> (Summary, Vec<EngineEvent>) {
    run_engine_with_cancel(cfg, Arc::new(AtomicBool::new(false)))
}

fn run_engine_with_cancel(cfg: Config, cancel: Arc<AtomicBool>) -> (Summary, Vec<EngineEvent>) {
    let (mut engine, rx) = Engine::new(cfg, cancel);
    let summary = engine.run();
    let events: Vec<EngineEvent> = rx.try_iter().collect();
    (summary, events)
}

/// 目标目录里所有文件（相对路径，排序）。
fn list_files(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if !root.exists() {
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.insert(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out
}

fn records(events: &[EngineEvent]) -> Vec<&filecopy_rs::model::CopyRecord> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Record(r) => Some(r),
            _ => None,
        })
        .collect()
}

fn logs(events: &[EngineEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Log(l) => Some(l.as_str()),
            _ => None,
        })
        .collect()
}

// ======================== 复制基本行为 ========================

#[test]
fn tree_strategy_preserves_directory_structure() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("DCIM/100CANON/IMG_0001.JPG"), b"aaaa");
    write_file(&src.join("DCIM/101CANON/IMG_0002.JPG"), b"bbbb");

    let cfg = base_config(&src, &dst, "tree");
    let (summary, _) = run_engine(cfg);

    assert_eq!(summary.scanned_files, 2);
    assert_eq!(summary.copied_files, 2);
    assert_eq!(summary.failed_files, 0);
    assert_eq!(
        list_files(&dst),
        BTreeSet::from([
            "DCIM/100CANON/IMG_0001.JPG".to_string(),
            "DCIM/101CANON/IMG_0002.JPG".to_string(),
        ])
    );
    assert_eq!(
        std::fs::read(dst.join("DCIM/100CANON/IMG_0001.JPG")).unwrap(),
        b"aaaa"
    );
}

#[test]
fn flat_strategy_puts_everything_in_target_root() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a/x.jpg"), b"1");
    write_file(&src.join("b/y.jpg"), b"2");

    let (summary, _) = run_engine(base_config(&src, &dst, "flat"));

    assert_eq!(summary.copied_files, 2);
    assert_eq!(
        list_files(&dst),
        BTreeSet::from(["x.jpg".to_string(), "y.jpg".to_string()])
    );
}

#[test]
fn ext_strategy_buckets_by_lowercase_extension_and_handles_missing_ext() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("IMG.JPG"), b"1");
    write_file(&src.join("RAW.NEF"), b"22");
    write_file(&src.join("README"), b"333");

    let (summary, _) = run_engine(base_config(&src, &dst, "ext"));

    assert_eq!(summary.copied_files, 3);
    assert_eq!(
        list_files(&dst),
        BTreeSet::from([
            "jpg/IMG.JPG".to_string(),
            "nef/RAW.NEF".to_string(),
            "no_ext/README".to_string(),
        ])
    );
}

#[test]
fn date_strategy_puts_files_under_today_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("sub/IMG_0001.JPG"), b"1");

    let (summary, _) = run_engine(base_config(&src, &dst, "date"));

    assert_eq!(summary.copied_files, 1);
    let today = chrono::Local::now().format("%Y/%m/%d").to_string();
    assert_eq!(
        list_files(&dst),
        BTreeSet::from([format!("{today}/IMG_0001.JPG")])
    );
}

#[test]
fn date_source_can_use_the_modified_time() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    let file = src.join("a.bin");
    write_file(&file, b"1");

    // 把修改时间固定到 2020-01-02 03:04:05，创建时间不可能等于它
    let stamp = filetime::FileTime::from_unix_time(1_577_934_245, 0);
    filetime::set_file_mtime(&file, stamp).unwrap();

    let mut cfg = base_config(&src, &dst, "date");
    cfg.date_source = "modified".to_string();
    cfg.date_format = "%Y-%m-%d".to_string();
    let (summary, events) = run_engine(cfg);

    assert_eq!(summary.copied_files, 1);
    assert_eq!(
        list_files(&dst),
        BTreeSet::from(["2020-01-02/a.bin".to_string()])
    );
    let logs: String = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::Log(text) => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        logs.contains("按修改日期"),
        "日志应说明用的是修改日期:\n{logs}"
    );
}

#[test]
fn unknown_date_source_falls_back_to_created() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a.bin"), b"1");

    let mut cfg = base_config(&src, &dst, "date");
    cfg.date_source = "拍摄日期".to_string();
    // 配置校验会拦住它，但引擎层也不能崩，按创建日期走
    let (summary, _) = run_engine(cfg);
    assert_eq!(summary.copied_files, 1);
}

#[test]
fn date_tree_strategy_keeps_structure_under_date_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("DCIM/100CANON/IMG_0001.JPG"), b"1");

    let (summary, _) = run_engine(base_config(&src, &dst, "date_tree"));

    assert_eq!(summary.copied_files, 1);
    let today = chrono::Local::now().format("%Y/%m/%d").to_string();
    assert_eq!(
        list_files(&dst),
        BTreeSet::from([format!("{today}/DCIM/100CANON/IMG_0001.JPG")])
    );
}

// ======================== 过滤 ========================

#[test]
fn extensions_filter_limits_scanned_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a.jpg"), b"1");
    write_file(&src.join("b.nef"), b"2");
    write_file(&src.join("c.txt"), b"3");

    let mut cfg = base_config(&src, &dst, "flat");
    cfg.extensions = vec!["jpg".into(), "nef".into()];
    let (summary, _) = run_engine(cfg);

    assert_eq!(summary.scanned_files, 2);
    assert_eq!(
        list_files(&dst),
        BTreeSet::from(["a.jpg".to_string(), "b.nef".to_string()])
    );
}

#[test]
fn exclude_patterns_skip_matching_file_names() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("keep.jpg"), b"1");
    write_file(&src.join("thumb_001.jpg"), b"2");

    let mut cfg = base_config(&src, &dst, "flat");
    cfg.exclude = vec!["thumb_*".into()];
    let (summary, _) = run_engine(cfg);

    assert_eq!(summary.scanned_files, 1);
    assert_eq!(list_files(&dst), BTreeSet::from(["keep.jpg".to_string()]));
}

#[test]
fn hidden_files_are_skipped_unless_enabled() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("visible.jpg"), b"1");
    write_file(&src.join(".hidden.jpg"), b"2");

    let (summary, _) = run_engine(base_config(&src, &dst, "flat"));
    assert_eq!(summary.scanned_files, 1);

    let dst2 = tmp.path().join("out2");
    let mut cfg = base_config(&src, &dst2, "flat");
    cfg.include_hidden = true;
    let (summary2, _) = run_engine(cfg);
    assert_eq!(summary2.scanned_files, 2);
    assert!(dst2.join(".hidden.jpg").exists());
}

#[test]
fn non_recursive_only_scans_top_level() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("top.jpg"), b"1");
    write_file(&src.join("deep/nested/inner.jpg"), b"2");

    let mut cfg = base_config(&src, &dst, "tree");
    cfg.recursive = false;
    let (summary, _) = run_engine(cfg);

    assert_eq!(summary.scanned_files, 1);
    assert_eq!(list_files(&dst), BTreeSet::from(["top.jpg".to_string()]));
}

// ======================== 预览 / 覆盖 / 跳过 ========================

#[test]
fn dry_run_writes_nothing_but_reports_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a.jpg"), b"1234");

    let mut cfg = base_config(&src, &dst, "flat");
    cfg.dry_run = true;
    let (summary, events) = run_engine(cfg);

    assert!(summary.dry_run);
    assert_eq!(summary.planned_files, 1);
    assert_eq!(summary.copied_files, 0);
    assert!(!dst.exists(), "dry-run 不应该创建目标目录");
    assert!(logs(&events).iter().any(|l| l.contains("DRY-RUN")));
    assert!(logs(&events).iter().any(|l| l.contains("a.jpg")));

    // 预览也要推进进度，否则界面上的文件数/数据量一直停在 0
    let progress: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Progress {
                files_done,
                files_total,
                bytes_done,
                bytes_total,
            } => Some((*files_done, *files_total, *bytes_done, *bytes_total)),
            _ => None,
        })
        .collect();
    assert!(!progress.is_empty(), "dry-run 应该上报进度");
    let last = *progress.last().unwrap();
    assert_eq!(last.0, 1, "文件进度应走完");
    assert_eq!(last.1, 1);
    assert_eq!(last.2, 4, "数据量进度应等于计划字节数");
    assert_eq!(last.3, 4);
}

#[test]
fn existing_target_is_skipped_without_overwrite() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a.jpg"), b"new content");
    write_file(&dst.join("a.jpg"), b"old");

    let (summary, events) = run_engine(base_config(&src, &dst, "flat"));

    assert_eq!(summary.skipped_files, 1);
    assert_eq!(summary.copied_files, 0);
    // 原文件保持不动
    assert_eq!(std::fs::read(dst.join("a.jpg")).unwrap(), b"old");
    // 跳过要说明原因（大小不同时提示内容可能不同）
    let skipped: Vec<_> = records(&events)
        .into_iter()
        .filter(|r| r.action == FileAction::Skipped)
        .collect();
    assert_eq!(skipped.len(), 1);
    assert!(
        skipped[0].detail.contains("目标已存在"),
        "跳过明细应说明目标已存在，实际: {}",
        skipped[0].detail
    );
}

#[test]
fn overwrite_replaces_existing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a.jpg"), b"new content");
    write_file(&dst.join("a.jpg"), b"old");

    let mut cfg = base_config(&src, &dst, "flat");
    cfg.overwrite = true;
    let (summary, _) = run_engine(cfg);

    assert_eq!(summary.overrode_files, 1);
    assert_eq!(summary.copied_files, 0);
    assert_eq!(std::fs::read(dst.join("a.jpg")).unwrap(), b"new content");
}

#[test]
fn leave_no_temp_files_behind_after_success() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a.jpg"), b"1234");

    let (summary, _) = run_engine(base_config(&src, &dst, "flat"));
    assert_eq!(summary.copied_files, 1);

    let leftovers: Vec<PathBuf> = std::fs::read_dir(&dst)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with(".filecopy_"))
                .unwrap_or(false)
        })
        .collect();
    assert!(leftovers.is_empty(), "临时文件残留: {leftovers:?}");
}

// ======================== 重名冲突 ========================

#[test]
fn duplicate_targets_are_skipped_when_not_auto_renaming() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a/IMG_0001.JPG"), b"first");
    write_file(&src.join("b/IMG_0001.JPG"), b"second");

    // flat 策略下两个文件映射到同一目标
    let (summary, events) = run_engine(base_config(&src, &dst, "flat"));

    assert_eq!(summary.copied_files, 1);
    assert_eq!(summary.skipped_files, 1);
    assert_eq!(list_files(&dst).len(), 1);
    assert!(
        logs(&events).iter().any(|l| l.contains("目标路径冲突")),
        "应提示目标路径冲突"
    );
    let dup = records(&events)
        .into_iter()
        .find(|r| r.action == FileAction::Skipped)
        .expect("应有一条跳过记录");
    assert!(
        dup.detail.contains("映射到同一目标"),
        "跳过明细应说明目标占用，实际: {}",
        dup.detail
    );
}

#[test]
fn auto_rename_keeps_both_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a/IMG_0001.JPG"), b"first");
    write_file(&src.join("b/IMG_0001.JPG"), b"second");

    let mut cfg = base_config(&src, &dst, "flat");
    cfg.auto_rename = true;
    let (summary, _) = run_engine(cfg);

    assert_eq!(summary.copied_files, 2);
    assert_eq!(summary.skipped_files, 0);
    assert_eq!(
        list_files(&dst),
        BTreeSet::from(["IMG_0001.JPG".to_string(), "IMG_0001_1.JPG".to_string()])
    );
}

#[test]
fn auto_rename_avoids_names_already_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a.jpg"), b"new");
    write_file(&dst.join("a.jpg"), b"old");
    write_file(&dst.join("a_1.jpg"), b"older");

    let mut cfg = base_config(&src, &dst, "flat");
    cfg.auto_rename = true;
    let (summary, _) = run_engine(cfg);

    assert_eq!(summary.copied_files, 1);
    // 磁盘上 a.jpg / a_1.jpg 都被占，新文件落到 a_2.jpg
    assert_eq!(
        list_files(&dst),
        BTreeSet::from([
            "a.jpg".to_string(),
            "a_1.jpg".to_string(),
            "a_2.jpg".to_string()
        ])
    );
    assert_eq!(std::fs::read(dst.join("a_2.jpg")).unwrap(), b"new");
}

// ======================== 校验和 ========================

#[test]
fn verify_counts_verified_files_and_notes_algorithm() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a.bin"), &vec![7u8; 4096]);

    let mut cfg = base_config(&src, &dst, "flat");
    cfg.verify = true;
    cfg.verify_algorithm = "blake2b".into();
    let (summary, events) = run_engine(cfg);

    assert_eq!(summary.copied_files, 1);
    assert_eq!(summary.verified_files, 1);
    assert!(records(&events)
        .iter()
        .any(|r| r.detail.contains("blake2b 校验通过")));
}

// ======================== 并行 / 缓冲区 ========================

#[test]
fn parallel_workers_produce_same_result_as_serial() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    for i in 0..12 {
        write_file(
            &src.join(format!("d{}/f{i}.jpg", i % 3)),
            format!("content-{i}").as_bytes(),
        );
    }

    let dst_serial = tmp.path().join("out-serial");
    let dst_parallel = tmp.path().join("out-parallel");

    let (serial, _) = run_engine(base_config(&src, &dst_serial, "tree"));

    let mut cfg = base_config(&src, &dst_parallel, "tree");
    cfg.workers = 4;
    let (parallel, _) = run_engine(cfg);

    assert_eq!(serial.copied_files, 12);
    assert_eq!(parallel.copied_files, 12);
    assert_eq!(list_files(&dst_serial), list_files(&dst_parallel));
}

#[test]
fn small_custom_buffer_still_copies_exact_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    write_file(&src.join("big.bin"), &payload);

    let mut cfg = base_config(&src, &dst, "flat");
    cfg.buffer_size = 1024; // 强制走手写缓冲循环
    let (summary, _) = run_engine(cfg);

    assert_eq!(summary.copied_files, 1);
    assert_eq!(std::fs::read(dst.join("big.bin")).unwrap(), payload);
    assert_eq!(summary.copied_bytes, payload.len() as u64);
}

// ======================== 进度口径 ========================

#[test]
fn progress_denominator_matches_bytes_that_will_be_written() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    write_file(&src.join("a.jpg"), b"12345");
    write_file(&src.join("b.jpg"), b"678");

    let (summary, events) = run_engine(base_config(&src, &dst, "flat"));

    assert_eq!(summary.pending_bytes, 8);
    let last_progress = events
        .iter()
        .rev()
        .find_map(|e| match e {
            EngineEvent::Progress {
                bytes_done,
                bytes_total,
                ..
            } => Some((*bytes_done, *bytes_total)),
            _ => None,
        })
        .expect("应有进度事件");
    assert_eq!(last_progress, (8, 8));
    assert_eq!(summary.copied_bytes, 8);
}

#[test]
fn every_planned_file_gets_exactly_one_record() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    for i in 0..8 {
        write_file(&src.join(format!("f{i}.jpg")), b"x");
    }

    let mut cfg = base_config(&src, &dst, "flat");
    cfg.workers = 3;
    let (summary, events) = run_engine(cfg);

    let recs = records(&events);
    assert_eq!(recs.len(), summary.planned_files);
    assert_eq!(recs.len(), 8);
    let unique: BTreeSet<_> = recs.iter().map(|r| r.src.clone()).collect();
    assert_eq!(unique.len(), 8, "同一条记录不应重复上报");
}

// ======================== 异常与退出码 ========================

#[test]
fn empty_scan_exit_code_depends_on_fail_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    std::fs::create_dir_all(&src).unwrap();
    let dst = tmp.path().join("out");

    let (summary, _) = run_engine(base_config(&src, &dst, "flat"));
    assert!(summary.empty_scan);
    assert_eq!(summary.exit_code(false), 0);
    assert_eq!(summary.exit_code(true), EXIT_ERROR);

    let mut cfg = base_config(&src, &dst, "flat");
    cfg.fail_empty = true;
    let (summary2, _) = run_engine(cfg);
    assert_eq!(summary2.exit_code(true), EXIT_ERROR);
}

#[test]
fn target_path_that_is_a_file_fails_each_file_instead_of_panicking() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let target_as_file = tmp.path().join("not-a-dir");
    write_file(&src.join("a.jpg"), b"1");
    write_file(&target_as_file, b"i am a file");

    let (summary, _) = run_engine(base_config(&src, &target_as_file, "flat"));

    assert_eq!(summary.failed_files, 1);
    assert_eq!(summary.exit_code(false), EXIT_ERROR);
    // 原来的文件不能被破坏
    assert_eq!(std::fs::read(&target_as_file).unwrap(), b"i am a file");
}

#[test]
fn cancelled_run_reports_unprocessed_files_and_signal_exit_code() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("card");
    let dst = tmp.path().join("out");
    for i in 0..5 {
        write_file(&src.join(format!("f{i}.jpg")), b"data");
    }

    // 一开始就置位：所有文件都应报「已中断，未被处理」
    let cancel = Arc::new(AtomicBool::new(true));
    let (summary, events) = run_engine_with_cancel(base_config(&src, &dst, "flat"), cancel);

    assert!(summary.cancelled);
    assert_eq!(summary.copied_files, 0);
    assert_eq!(summary.failed_files, 5);
    assert_eq!(summary.exit_code(false), EXIT_SIGINT);
    let failed: Vec<_> = records(&events)
        .into_iter()
        .filter(|r| r.action == FileAction::Failed)
        .collect();
    assert_eq!(failed.len(), 5);
    assert!(failed.iter().all(|r| r.detail.contains("已中断")));
    // 中断不该留下任何目标文件
    assert!(list_files(&dst).is_empty());
}

#[test]
fn unreadable_source_directory_is_reported_not_silent() {
    // 源目录不存在时不应 panic；扫描结果为空且被明确告知
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does-not-exist");
    let dst = tmp.path().join("out");

    let (summary, events) = run_engine(base_config(&missing, &dst, "flat"));
    assert_eq!(summary.scanned_files, 0);
    assert!(summary.empty_scan);
    assert!(!logs(&events).is_empty(), "至少应有扫描结果日志");
}
