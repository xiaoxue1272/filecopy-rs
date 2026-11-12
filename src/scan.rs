//! 扫描与过滤：遍历源目录，产出待复制的文件列表。
//!
//! 三个刻意的选择：
//! 1. 不跟随软链接目录，避免环形遍历与「把外部内容复制进来」；
//! 2. 目录名与文件名都排序 —— `read_dir` 的顺序是文件系统的任意顺序，
//!    不排序会让「记录列表」每次都不一样，dry-run 预览也和真实复制对不上；
//! 3. 不可读目录要收集起来上报。`Path::read_dir` 的递归如果直接吞掉错误，
//!    用户只会看到「计划里少了一半文件」而没有任何提示。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 过滤条件（对应 Python 的 `FileFilter`）。
#[derive(Debug, Clone)]
pub struct FileFilter {
    pub extensions: Vec<String>,
    pub exclude: Vec<String>,
    pub include_hidden: bool,
    pub recursive: bool,
    pub source_root: Option<PathBuf>,
}

impl FileFilter {
    pub fn new(
        extensions: Vec<String>,
        exclude: Vec<String>,
        include_hidden: bool,
        recursive: bool,
        source_root: Option<PathBuf>,
    ) -> Self {
        Self {
            extensions,
            exclude,
            include_hidden,
            recursive,
            source_root,
        }
    }

    /// 后缀集合只算一次：`should_copy` 对每个文件都要用。
    pub(crate) fn ext_set(&self) -> HashSet<String> {
        self.extensions
            .iter()
            .map(|e| {
                let lower = e.to_lowercase();
                if lower.starts_with('.') {
                    lower
                } else {
                    format!(".{lower}")
                }
            })
            .collect()
    }

    pub fn should_copy(&self, path: &Path, ext_set: &HashSet<String>) -> bool {
        if !path.is_file() {
            return false;
        }
        if !self.include_hidden && self.is_hidden(path) {
            return false;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if self.exclude.iter().any(|pattern| fnmatch(&name, pattern)) {
            return false;
        }
        if ext_set.is_empty() {
            return true; // 留空 = 全部
        }
        let suffix = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        ext_set.contains(&suffix)
    }

    /// 只看「源目录之内」的部分：源目录本身位于点目录（如 `~/.config`）时，
    /// 不该把里面所有文件都误判成隐藏文件。
    fn relative_parts(&self, path: &Path) -> Vec<String> {
        if let Some(root) = &self.source_root {
            if let Ok(rel) = path.strip_prefix(root) {
                return rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect();
            }
        }
        vec![path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()]
    }

    fn is_hidden(&self, path: &Path) -> bool {
        if self
            .relative_parts(path)
            .iter()
            .any(|part| part.starts_with('.'))
        {
            return true;
        }
        #[cfg(windows)]
        {
            if let Ok(meta) = std::fs::metadata(path) {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
                return meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0;
            }
        }
        false
    }
}

/// 预览用的「第一个待复制文件」。
///
/// 遍历顺序与 `scan_sources` 完全一致（目录名、文件名都排序），
/// 所以预览里那个文件，就是真正开始复制时会处理的第一个文件。
/// 找不到（空目录 / 过滤条件全不匹配 / 读不了）时返回 `None`。
///
/// 每帧都可能调用，所以给了预算：极端情况下（几十万条目的坏介质）
/// 宁可放弃预览，也不要让界面卡住。
pub fn first_matching_file(root: &Path, filter: &FileFilter) -> Option<PathBuf> {
    let ext_set = filter.ext_set();
    let mut budget = SAMPLE_SCAN_BUDGET;
    sample_walk(root, filter, &ext_set, &mut budget, 0)
}

/// 预览扫描最多看这么多个目录项；超过就放弃（退回占位示例）。
const SAMPLE_SCAN_BUDGET: usize = 50_000;

fn sample_walk(
    dir: &Path,
    filter: &FileFilter,
    ext_set: &HashSet<String>,
    budget: &mut usize,
    depth: usize,
) -> Option<PathBuf> {
    if depth > 64 || *budget == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;

    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            if path.is_file() {
                files.push(path);
            }
            continue;
        }
        if ft.is_dir() {
            subdirs.push(path);
        } else if ft.is_file() {
            files.push(path);
        }
    }

    subdirs.sort();
    files.sort();

    for file in files {
        if filter.should_copy(&file, ext_set) {
            return Some(file);
        }
    }
    if !filter.recursive {
        return None;
    }
    for sub in subdirs {
        if let Some(hit) = sample_walk(&sub, filter, ext_set, budget, depth + 1) {
            return Some(hit);
        }
    }
    None
}

/// 一次扫描的结果。
#[derive(Debug, Default)]
pub struct ScanOutcome {
    pub files: Vec<PathBuf>,
    /// 读不了的目录（权限等），要如实上报。
    pub unreadable_dirs: Vec<String>,
}

/// 按过滤条件遍历 `root`。
pub fn scan_sources(root: &Path, filter: &FileFilter) -> ScanOutcome {
    let ext_set = filter.ext_set();
    let mut outcome = ScanOutcome::default();
    walk(root, filter, &ext_set, &mut outcome, &mut Vec::new());
    outcome
}

fn walk(
    dir: &Path,
    filter: &FileFilter,
    ext_set: &HashSet<String>,
    outcome: &mut ScanOutcome,
    stack: &mut Vec<PathBuf>,
) {
    // 软链接环兜底：即便不跟随目录，硬链接/绑定挂载仍可能形成环
    if stack.len() > 64 {
        outcome
            .unreadable_dirs
            .push(format!("{}（目录层级过深，已跳过）", dir.display()));
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            outcome
                .unreadable_dirs
                .push(dir.to_string_lossy().to_string());
            return;
        }
    };

    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                outcome
                    .unreadable_dirs
                    .push(dir.to_string_lossy().to_string());
                continue;
            }
        };
        let path = entry.path();
        // 用 symlink_metadata：不跟随软链接，才能判断「本身」是不是链接
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            // 软链接文件本身可以复制；指向目录的软链接不跟随（避免环）
            if path.is_file() {
                files.push(path);
            }
            continue;
        }
        if ft.is_dir() {
            subdirs.push(path);
        } else if ft.is_file() {
            files.push(path);
        }
    }

    subdirs.sort();
    files.sort();

    for file in files {
        if filter.should_copy(&file, ext_set) {
            outcome.files.push(file);
        }
    }

    if !filter.recursive {
        // 非递归只看源目录本身，其余层级整棵剪掉
        return;
    }
    stack.push(dir.to_path_buf());
    for sub in subdirs {
        walk(&sub, filter, ext_set, outcome, stack);
    }
    stack.pop();
}

/// `fnmatch` 的等价实现：`*`、`?`、`[seq]`、`[!seq]`。
///
/// 用来匹配用户填的排除规则（如 `*.tmp`、`IMG_????.jpg`、`[0-9]*`）。
pub fn fnmatch(name: &str, pattern: &str) -> bool {
    let name: Vec<char> = if cfg!(windows) {
        name.to_lowercase().chars().collect()
    } else {
        name.chars().collect()
    };
    let pattern: Vec<char> = if cfg!(windows) {
        pattern.to_lowercase().chars().collect()
    } else {
        pattern.chars().collect()
    };
    match_here(&name, 0, &pattern, 0)
}

fn match_here(name: &[char], mut ni: usize, pat: &[char], mut pi: usize) -> bool {
    while pi < pat.len() {
        match pat[pi] {
            '*' => {
                // 折叠连续的 '*'
                while pi < pat.len() && pat[pi] == '*' {
                    pi += 1;
                }
                if pi == pat.len() {
                    return true;
                }
                let mut k = ni;
                while k <= name.len() {
                    if match_here(name, k, pat, pi) {
                        return true;
                    }
                    k += 1;
                }
                return false;
            }
            '?' => {
                if ni >= name.len() {
                    return false;
                }
                ni += 1;
                pi += 1;
            }
            '[' => {
                if ni >= name.len() {
                    return false;
                }
                let (ok, next_pi) = match_class(name[ni], pat, pi);
                match next_pi {
                    Some(next) => {
                        if !ok {
                            return false;
                        }
                        ni += 1;
                        pi = next;
                    }
                    None => {
                        // 未闭合的 '[' 当字面量处理（与 fnmatch 一致）
                        if name[ni] != '[' {
                            return false;
                        }
                        ni += 1;
                        pi += 1;
                    }
                }
            }
            c => {
                if ni >= name.len() || name[ni] != c {
                    return false;
                }
                ni += 1;
                pi += 1;
            }
        }
    }
    ni == name.len()
}

/// 解析 `[...]` 字符类，返回 (是否匹配, 结束后的位置)。
fn match_class(ch: char, pat: &[char], start: usize) -> (bool, Option<usize>) {
    let mut i = start + 1;
    let mut negate = false;
    if i < pat.len() && (pat[i] == '!' || pat[i] == '^') {
        negate = true;
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pat.len() {
        let c = pat[i];
        if c == ']' && !first {
            return (matched != negate, Some(i + 1));
        }
        first = false;
        // 区间 a-z
        if i + 2 < pat.len() && pat[i + 1] == '-' && pat[i + 2] != ']' {
            let (lo, hi) = (c, pat[i + 2]);
            if lo <= ch && ch <= hi {
                matched = true;
            }
            i += 3;
            continue;
        }
        if c == ch {
            matched = true;
        }
        i += 1;
    }
    (false, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
        dir
    }

    fn names(outcome: &ScanOutcome, root: &Path) -> Vec<String> {
        let mut v: Vec<String> = outcome
            .files
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn fnmatch_basics() {
        assert!(fnmatch("IMG_0001.JPG", "*.jpg".to_uppercase().as_str()));
        assert!(fnmatch("a.tmp", "*.tmp"));
        assert!(!fnmatch("a.txt", "*.tmp"));
        assert!(fnmatch("IMG_0001.JPG", "IMG_????.JPG"));
        assert!(fnmatch("7.jpg", "[0-9].jpg"));
        assert!(!fnmatch("x.jpg", "[0-9].jpg"));
        assert!(fnmatch("x.jpg", "[!0-9].jpg"));
        assert!(fnmatch("abc", "a*c"));
        assert!(fnmatch("abc", "a[b]c"));
    }

    #[test]
    fn scans_recursively_and_sorted() {
        let dir = tree(&[
            ("DCIM/b/2.jpg", "x"),
            ("DCIM/a/1.jpg", "x"),
            ("top.jpg", "x"),
        ]);
        let filter = FileFilter::new(vec![], vec![], false, true, Some(dir.path().into()));
        let outcome = scan_sources(dir.path(), &filter);
        assert_eq!(
            names(&outcome, dir.path()),
            vec!["DCIM/a/1.jpg", "DCIM/b/2.jpg", "top.jpg"]
        );
    }

    #[test]
    fn non_recursive_only_top_level() {
        let dir = tree(&[("DCIM/a/1.jpg", "x"), ("top.jpg", "x")]);
        let filter = FileFilter::new(vec![], vec![], false, false, Some(dir.path().into()));
        let outcome = scan_sources(dir.path(), &filter);
        assert_eq!(names(&outcome, dir.path()), vec!["top.jpg"]);
    }

    #[test]
    fn filters_extensions_hidden_and_exclude() {
        let dir = tree(&[
            ("a.JPG", "x"),
            ("b.CR2", "x"),
            (".hidden.jpg", "x"),
            ("skip.tmp.jpg", "x"),
            ("c.txt", "x"),
        ]);
        let filter = FileFilter::new(
            vec!["jpg".into()],
            vec!["skip*".into()],
            false,
            true,
            Some(dir.path().into()),
        );
        let outcome = scan_sources(dir.path(), &filter);
        assert_eq!(names(&outcome, dir.path()), vec!["a.JPG"]);

        let filter_all = FileFilter::new(vec![], vec![], false, true, Some(dir.path().into()));
        let outcome = scan_sources(dir.path(), &filter_all);
        assert_eq!(
            names(&outcome, dir.path()),
            vec!["a.JPG", "b.CR2", "c.txt", "skip.tmp.jpg"]
        );
    }

    #[test]
    fn hidden_source_root_does_not_hide_everything() {
        let dir = tree(&[(".config/proj/photo.jpg", "x")]);
        let root = dir.path().join(".config/proj");
        let filter = FileFilter::new(vec![], vec![], false, true, Some(root.clone()));
        let outcome = scan_sources(&root, &filter);
        assert_eq!(names(&outcome, &root), vec!["photo.jpg"]);
    }
}
