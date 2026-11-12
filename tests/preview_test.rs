//! 界面上的「示例落点」必须和真跑一遍得到的结果一致。
//!
//! 这里不测界面，而是直接拿预览和引擎的规划结果对账：同一份源目录、同一套
//! 设置，预览给出的相对路径必须等于 `Plan::raw` 给同一个文件算出来的相对路径。
//! 只要预览偷偷用了假数据（写死日期、编个文件名），这些用例就会挂。

use filecopy_rs::config::{parse_extensions, Config};
use filecopy_rs::engine::{Engine, EngineEvent};
use filecopy_rs::model::{DateSource, Strategy};
use filecopy_rs::plan::Plan;
use filecopy_rs::preview::{self, PreviewInput};
use filecopy_rs::scan::{self, FileFilter};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// 造一棵有点层次的源目录：根目录有文件，子目录里也有，另有一个不该被收的。
fn sample_card() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("卡");
    std::fs::create_dir_all(root.join("相册").join("旅行")).unwrap();
    std::fs::create_dir_all(root.join("视频")).unwrap();
    std::fs::write(root.join("首张.jpg"), b"x").unwrap();
    std::fs::write(root.join("相册").join("旅行").join("样片.jpg"), b"x").unwrap();
    std::fs::write(root.join("视频").join("片段.mp4"), b"x").unwrap();
    std::fs::write(root.join("笔记.txt"), b"x").unwrap();
    temp
}

fn filter(root: &Path) -> FileFilter {
    FileFilter::new(
        parse_extensions("jpg,mp4"),
        Vec::new(),
        false,
        true,
        Some(root.to_path_buf()),
    )
}

fn relative(dst: &Path, dst_root: &Path) -> String {
    dst.strip_prefix(dst_root)
        .unwrap_or(dst)
        .display()
        .to_string()
}

#[test]
fn sampled_file_is_the_first_file_the_real_scan_returns() {
    let temp = sample_card();
    let root = temp.path().join("卡");
    let f = filter(&root);

    let first = scan::first_matching_file(&root, &f).expect("源目录里有匹配文件");
    let scanned = scan::scan_sources(&root, &f);
    assert_eq!(
        Some(first),
        scanned.files.first().cloned(),
        "预览取样必须和真正扫描的第一个文件一致，否则示例和实际落点会错位"
    );
}

#[test]
fn preview_equals_the_real_plan_for_every_strategy() {
    let temp = sample_card();
    let root = temp.path().join("卡");
    let dst_root: PathBuf = temp.path().join("备份");
    let f = filter(&root);
    let warnings = &mut Vec::new();

    for strategy in Strategy::ALL {
        for fmt in ["%Y/%m/%d", "%Y-%m", "%Y"] {
            let preview = preview::compute(&PreviewInput {
                strategy,
                src_root: &root,
                dst_root: &dst_root,
                date_format: fmt,
                date_source: DateSource::Created,
                filter: &f,
                ext: Some("jpg"),
            });
            assert!(
                preview.from_real_file,
                "{strategy:?}/{fmt}: 源目录里有 jpg，不该退化成占位示例"
            );

            let files = scan::scan_sources(&root, &f).files;
            let raw = Plan::raw(
                &files,
                strategy,
                &root,
                &dst_root,
                fmt,
                DateSource::Created,
                warnings,
            );
            let first = &raw[0];
            assert_eq!(
                preview.relative,
                relative(&first.dst, &dst_root),
                "{strategy:?}/{fmt}: 预览和真实规划不一致"
            );
        }
    }
}

#[test]
fn preview_follows_the_chosen_date_source() {
    let temp = sample_card();
    let root = temp.path().join("卡");
    let dst_root = temp.path().join("备份");
    let f = filter(&root);

    // 把一个文件的修改时间钉死到很久以前，日期目录就该跟着落到那一年
    let target = root.join("首张.jpg");
    let stamp = filetime::FileTime::from_unix_time(1_600_000_000, 0); // 2020-09-13
    filetime::set_file_mtime(&target, stamp).unwrap();

    let preview = preview::compute(&PreviewInput {
        strategy: Strategy::Date,
        src_root: &root,
        dst_root: &dst_root,
        date_format: "%Y/%m",
        date_source: DateSource::Modified,
        filter: &f,
        ext: Some("jpg"),
    });
    assert_eq!(preview.relative, "2020/09/首张.jpg");
}

#[test]
fn preview_respects_non_recursive_scanning() {
    let temp = sample_card();
    let root = temp.path().join("卡");
    let dst_root = temp.path().join("备份");

    // 只看根层时，第一个匹配文件是根目录的 首张.jpg
    let flat_only = FileFilter::new(
        parse_extensions("jpg,mp4"),
        Vec::new(),
        false,
        false,
        Some(root.clone()),
    );
    let preview = preview::compute(&PreviewInput {
        strategy: Strategy::Date,
        src_root: &root,
        dst_root: &dst_root,
        date_format: "%Y/%m",
        date_source: DateSource::Created,
        filter: &flat_only,
        ext: Some("jpg"),
    });
    let scanned = scan::scan_sources(&root, &flat_only);
    let raw = Plan::raw(
        &scanned.files,
        Strategy::Date,
        &root,
        &dst_root,
        "%Y/%m",
        DateSource::Created,
        &mut Vec::new(),
    );
    assert_eq!(preview.relative, relative(&raw[0].dst, &dst_root));
    assert!(preview.relative.ends_with("首张.jpg"));
}

/// 引擎 dry-run 打出来的第一条落点（真正跑一遍规划、不写盘）。
fn first_dry_run_dst(cfg: &Config) -> String {
    let (mut engine, rx) = Engine::new(cfg.clone(), Arc::new(AtomicBool::new(false)));
    engine.run();
    for event in rx.try_iter() {
        if let EngineEvent::Log(line) = event {
            // 计划预览的格式是 `源 → 目标  [大小]`
            if let Some((_, rest)) = line.split_once(" → ") {
                return rest.split("  [").next().unwrap_or(rest).to_string();
            }
        }
    }
    panic!("dry-run 一条落点都没打出来");
}

fn config_for(src: &Path, dst: &Path, strategy: Strategy, fmt: &str) -> Config {
    let mut cfg = Config {
        source_dir: src.display().to_string(),
        target_dir: dst.display().to_string(),
        extensions: parse_extensions("jpg,mp4"),
        strategy: strategy.name().to_string(),
        date_format: fmt.to_string(),
        dry_run: true,
        ..Default::default()
    };
    cfg.normalize_paths();
    cfg
}

/// 终极对账：界面上的「示例落点」== 真按这份配置 dry-run 出来的第一条落点。
#[test]
fn preview_matches_what_a_real_dry_run_plans() {
    let temp = sample_card();
    let root = temp.path().join("卡");
    let dst_root = temp.path().join("备份");

    for strategy in Strategy::ALL {
        for fmt in ["%Y/%m/%d", "%Y-%m", "%d-%m-%Y"] {
            let cfg = config_for(&root, &dst_root, strategy, fmt);
            let preview = preview::compute_from_config(&cfg);
            let planned = first_dry_run_dst(&cfg);
            assert_eq!(
                preview.relative, planned,
                "{strategy:?}/{fmt}: 预览和真跑出来的落点不一样"
            );
        }
    }
}

/// 源目录写着 `~` / 带引号时，预览要和引擎一样先展开再算。
#[test]
fn preview_expands_the_same_paths_the_engine_does() {
    let temp = sample_card();
    let root = temp.path().join("卡");
    let dst_root = temp.path().join("备份");

    let mut cfg = config_for(&root, &dst_root, Strategy::Date, "%Y/%m");
    // Finder 拷贝路径会带引号，手输常带空格
    cfg.source_dir = format!("  \"{}\"  ", root.display());
    cfg.target_dir = format!("  {}  ", dst_root.display());
    cfg.normalize_paths();

    let preview = preview::compute_from_config(&cfg);
    assert!(preview.from_real_file, "展开后应该能看到真实文件");
    assert_eq!(preview.relative, first_dry_run_dst(&cfg));
    assert!(!preview.relative.contains('"'));
}
