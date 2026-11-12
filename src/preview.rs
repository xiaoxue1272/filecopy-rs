//! 「示例落点」的真实计算。
//!
//! 以前界面上的示例是拼出来的字符串（`2026/11/08/文件.jpg`），
//! 日期写死、格式改了也不变、策略细节还可能和引擎对不上，看着像那么回事，
//! 实际会误导人。这里改成**走真正的策略计算**：
//!
//! - 在源目录里找第一个真正会被复制的文件（遍历顺序与 `scan_sources` 一致）
//! - 用当前界面上的日期格式 / 日期来源 / 目标目录，调用 `Strategy::target_path`，和引擎规划阶段用的是同一份代码
//! - 找不到真实文件时退化成 `文件.<扩展名>` 放在源目录根下，仍然走同一套计算，所以日期格式、扩展名目录这些依然是真的
//!
//! 换句话说：这里显示的落点，就是「真按当前设置开始复制时，第一个文件会去哪」。

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::model::{DateSource, PathCtx, Strategy};
use crate::scan::{first_matching_file, FileFilter};

/// 预览结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    /// 相对目标目录的落点（目标目录为空时就是完整的落点路径）。
    pub relative: String,
    /// 是否取自源目录里真实存在的文件；`false` 表示用的是占位文件名。
    pub from_real_file: bool,
}

/// 计算预览需要的一切。
pub struct PreviewInput<'a> {
    pub strategy: Strategy,
    pub src_root: &'a Path,
    pub dst_root: &'a Path,
    pub date_format: &'a str,
    pub date_source: DateSource,
    pub filter: &'a FileFilter,
    /// 「扩展名」里填的第一个，只用来给占位文件名补后缀。
    pub ext: Option<&'a str>,
}

/// 直接用运行配置算预览。
///
/// 界面上一按「开始」/「预览」，引擎拿到的也是同一份 `Config`（已经过
/// `normalize_paths` 展开 `~`、环境变量、引号），所以这里和真跑看到的落点
/// 是同一套输入 + 同一套计算，不存在两套逻辑各算各的。
pub fn compute_from_config(cfg: &Config) -> Preview {
    let src_root = cfg.src_path();
    let dst_root = cfg.dst_path();
    let filter = FileFilter::new(
        cfg.extensions.clone(),
        cfg.exclude.clone(),
        cfg.include_hidden,
        cfg.recursive,
        Some(src_root.clone()),
    );
    let ext = cfg.extensions.first().cloned();
    compute(&PreviewInput {
        // 与 `Config::strategy()` 同义，但 `&Config` 不能调它（它会顺手改写自身）
        strategy: Strategy::from_name(&cfg.strategy).unwrap_or(Strategy::Date),
        src_root: &src_root,
        dst_root: &dst_root,
        date_format: &cfg.date_format,
        date_source: DateSource::from_name(&cfg.date_source).unwrap_or_default(),
        filter: &filter,
        ext: ext.as_deref(),
    })
}

/// 按真实逻辑算出第一个文件的落点。
pub fn compute(input: &PreviewInput<'_>) -> Preview {
    // 源目录都没填就别算了：返回空落点，界面显示「选好源目录后显示」
    if input.src_root.as_os_str().is_empty() {
        return Preview::default();
    }
    let sample = first_matching_file(input.src_root, input.filter);
    let from_real_file = sample.is_some();
    let src: PathBuf = sample.unwrap_or_else(|| placeholder_source(input.src_root, input.ext));

    // 警告在这里没地方显示（引擎跑起来时才会上报），丢掉即可：
    // 预览只关心「落点长什么样」，不重复报同一件事。
    let mut warnings = Vec::new();
    let mut ctx = PathCtx {
        src_root: input.src_root,
        dst_root: input.dst_root,
        date_format: input.date_format,
        date_source: input.date_source,
        warnings: &mut warnings,
    };
    let target = input.strategy.target_path(&src, &mut ctx);

    Preview {
        // 目标目录为空时 `strip_prefix` 会失败，直接显示完整落点
        relative: target
            .strip_prefix(input.dst_root)
            .unwrap_or(&target)
            .display()
            .to_string(),
        from_real_file,
    }
}

/// 没有真实文件时的占位路径：源目录根下的 `文件.<扩展名>`。
fn placeholder_source(src_root: &Path, ext: Option<&str>) -> PathBuf {
    let name = match ext {
        Some(ext) if !ext.is_empty() => format!("文件.{ext}"),
        _ => "文件".to_string(),
    };
    src_root.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn filter(extensions: &str, exclude: &str, root: &Path) -> FileFilter {
        FileFilter::new(
            crate::config::parse_extensions(extensions),
            if exclude.is_empty() {
                Vec::new()
            } else {
                exclude.split_whitespace().map(|s| s.to_string()).collect()
            },
            false,
            true,
            Some(root.to_path_buf()),
        )
    }

    fn input<'a>(
        strategy: Strategy,
        src: &'a Path,
        dst: &'a Path,
        fmt: &'a str,
        filter: &'a FileFilter,
        ext: Option<&'a str>,
    ) -> PreviewInput<'a> {
        PreviewInput {
            strategy,
            src_root: src,
            dst_root: dst,
            date_format: fmt,
            date_source: DateSource::Created,
            filter,
            ext,
        }
    }

    /// 日期一律按「今天」算，避免测试跨天时假失败
    fn today(fmt: &str) -> String {
        chrono::Local::now().format(fmt).to_string()
    }

    #[test]
    fn an_empty_config_has_no_preview_at_all() {
        // 源目录都没填：界面显示「选好源目录后显示」，这里必须是空落点
        let preview = compute_from_config(&Config::default());
        assert_eq!(preview.relative, "");
        assert!(!preview.from_real_file);
    }

    #[test]
    fn uses_the_first_real_file_and_the_configured_date_format() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("card");
        fs::create_dir_all(&src).unwrap();
        fs::File::create(src.join("b.jpg")).unwrap();
        fs::File::create(src.join("a.jpg")).unwrap();
        let f = filter("jpg", "", &src);
        let dst = Path::new("/out");

        // %Y-%m 只有两级：结果必须跟着格式走，而不是写死的 年/月/日
        let preview = compute(&input(Strategy::Date, &src, dst, "%Y-%m", &f, None));
        assert!(preview.from_real_file);
        assert_eq!(
            preview.relative,
            format!("{}/a.jpg", today("%Y-%m")),
            "应按真实文件 + 当前日期格式算出落点"
        );

        let preview = compute(&input(Strategy::Date, &src, dst, "%Y/%m/%d", &f, None));
        assert_eq!(
            preview.relative,
            format!("{}/a.jpg", today("%Y/%m/%d")),
            "换回年月日三段时，落点也要跟着变"
        );
    }

    #[test]
    fn keeps_the_source_structure_for_tree_strategies() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("card");
        let nested = src.join("相册/旅行");
        fs::create_dir_all(&nested).unwrap();
        fs::File::create(nested.join("x.jpg")).unwrap();
        let f = filter("jpg", "", &src);
        let dst = Path::new("/out");

        let preview = compute(&input(Strategy::DateTree, &src, dst, "%Y", &f, None));
        assert_eq!(
            preview.relative,
            format!("{}/相册/旅行/x.jpg", today("%Y")),
            "date_tree 要把源目录结构接在日期后面"
        );

        let preview = compute(&input(Strategy::Tree, &src, dst, "%Y", &f, None));
        assert_eq!(preview.relative, "相册/旅行/x.jpg");
    }

    #[test]
    fn ext_strategy_uses_the_real_extension_in_lowercase() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("card");
        fs::create_dir_all(&src).unwrap();
        fs::File::create(src.join("photo.NEF")).unwrap();
        let f = filter("nef", "", &src);
        let preview = compute(&input(
            Strategy::Ext,
            &src,
            Path::new("/out"),
            "%Y",
            &f,
            None,
        ));
        assert_eq!(preview.relative, "nef/photo.NEF");
        assert!(preview.from_real_file);
    }

    #[test]
    fn falls_back_to_a_placeholder_when_no_file_matches() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("card");
        fs::create_dir_all(&src).unwrap();
        fs::File::create(src.join("a.jpg")).unwrap();
        // 只要 nef：目录里没有，就用占位文件名，但日期仍然是真实计算出来的
        let f = filter("nef", "", &src);
        let preview = compute(&input(
            Strategy::Date,
            &src,
            Path::new("/out"),
            "%Y-%m",
            &f,
            Some("nef"),
        ));
        assert!(!preview.from_real_file);
        assert_eq!(preview.relative, format!("{}/文件.nef", today("%Y-%m")));
    }

    #[test]
    fn respects_exclude_rules() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("card");
        fs::create_dir_all(&src).unwrap();
        fs::File::create(src.join("a.tmp")).unwrap();
        fs::File::create(src.join("b.jpg")).unwrap();
        let f = filter("", "*.tmp", &src);
        let preview = compute(&input(
            Strategy::Flat,
            &src,
            Path::new("/out"),
            "%Y",
            &f,
            None,
        ));
        assert_eq!(preview.relative, "b.jpg", "被排除的文件不该出现在预览里");
    }

    #[test]
    fn dst_root_is_stripped_from_the_display() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("card");
        fs::create_dir_all(&src).unwrap();
        fs::File::create(src.join("a.jpg")).unwrap();
        let f = filter("jpg", "", &src);
        let preview = compute(&input(
            Strategy::Flat,
            &src,
            Path::new("/out/import"),
            "%Y",
            &f,
            None,
        ));
        assert_eq!(preview.relative, "a.jpg");

        // 目标目录没填时，落点就是全部（相对路径等于完整路径）
        let preview = compute(&input(Strategy::Flat, &src, Path::new(""), "%Y", &f, None));
        assert_eq!(preview.relative, "a.jpg");
    }
}
