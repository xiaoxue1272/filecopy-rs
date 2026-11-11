//! 计划：把「源文件列表」变成「源 → 目标」映射，处理重名冲突。
//!
//! 这里的关键不变量是：**同一个目标路径在本次计划里只能有一个赢家**。
//! 扁平策略下 `DCIM/100CANON/IMG_0001.JPG` 与 `DCIM/101CANON/IMG_0001.JPG`
//! 会映射到同一路径，并行复制时两个线程会同时写它 —— 后者覆盖前者，却双双
//! 计入成功，磁盘上只剩一张照片。所以即使没开自动改名，也要先「认领」目标：
//! 先到者占用，后来者登记为重复并明确报跳过，串行与并行结果因此一致。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::{display_path, norm_key, PathCtx, Strategy};

/// 计划里的一项：源文件与它的目标路径。
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub src: PathBuf,
    pub dst: PathBuf,
}

/// 一组映射到同一目标的源文件（冲突检测用）。
#[derive(Debug, Clone)]
pub struct Collision {
    pub dst: PathBuf,
    pub sources: Vec<PathBuf>,
}

/// 完整计划。
#[derive(Debug, Default)]
pub struct Plan {
    pub items: Vec<PlanItem>,
    /// 本次计划里被别的源文件抢走目标的文件 → 抢走它的那个源文件。
    pub duplicates: HashMap<PathBuf, PathBuf>,
    /// 改名之前的冲突分组；开了自动改名时它是「已改名的组」。
    pub collisions: Vec<Collision>,
}

impl Plan {
    /// 生成未经改名处理的 (源, 目标) 映射 —— 冲突检测必须基于它。
    pub fn raw(
        files: &[PathBuf],
        strategy: Strategy,
        src_root: &Path,
        dst_root: &Path,
        date_format: &str,
        date_source: crate::model::DateSource,
        warnings: &mut Vec<String>,
    ) -> Vec<PlanItem> {
        let mut out = Vec::with_capacity(files.len());
        for f in files {
            let mut ctx = PathCtx {
                src_root,
                dst_root,
                date_format,
                date_source,
                warnings,
            };
            let dst = strategy.target_path(f, &mut ctx);
            out.push(PlanItem {
                src: f.clone(),
                dst,
            });
        }
        out
    }

    /// 由 raw plan 生成最终计划：认领目标、可选自动改名、统计冲突。
    pub fn build(
        raw: Vec<PlanItem>,
        auto_rename: bool,
        overwrite: bool,
        keep_duplicates_in_items: bool,
    ) -> Plan {
        let mut collisions_map: HashMap<String, Collision> = HashMap::new();
        for item in &raw {
            let entry = collisions_map
                .entry(norm_key(&item.dst))
                .or_insert_with(|| Collision {
                    dst: item.dst.clone(),
                    sources: Vec::new(),
                });
            entry.sources.push(item.src.clone());
        }

        let mut plan = Plan {
            items: Vec::with_capacity(raw.len()),
            duplicates: HashMap::new(),
            collisions: collisions_map
                .into_values()
                .filter(|c| c.sources.len() > 1)
                .collect(),
        };
        plan.collisions.sort_by(|a, b| a.dst.cmp(&b.dst));

        let mut winners: HashMap<String, PathBuf> = HashMap::new();
        let mut reserved: std::collections::HashSet<String> = std::collections::HashSet::new();
        for item in raw {
            let key = norm_key(&item.dst);
            // 不开自动改名时，同一个目标只允许先到者；后来者在 duplicates 里明确报跳过
            if !auto_rename {
                if let Some(winner) = winners.get(&key) {
                    plan.duplicates.insert(item.src.clone(), winner.clone());
                    if keep_duplicates_in_items {
                        plan.items.push(item);
                    }
                    continue;
                }
            }
            winners.insert(key.clone(), item.src.clone());
            if auto_rename {
                let disk_occupied = item.dst.exists() && !overwrite;
                let dst = unique_target(&item.dst, &mut reserved, disk_occupied);
                plan.items.push(PlanItem { src: item.src, dst });
            } else {
                reserved.insert(key);
                plan.items.push(item);
            }
        }
        plan
    }

    /// 计划里真正会被写入的字节和（进度条分母的唯一判据）。
    pub fn pending_bytes<F>(&self, overwrite: bool, sizes: F) -> u64
    where
        F: Fn(&Path) -> u64,
    {
        self.items
            .iter()
            .filter(|item| self.will_write(item, overwrite))
            .map(|item| sizes(&item.src))
            .sum()
    }

    /// 这一项是否真的会写盘。
    pub fn will_write(&self, item: &PlanItem, overwrite: bool) -> bool {
        if self.duplicates.contains_key(&item.src) {
            return false; // 计划内被别人抢了同一目标 → 必然跳过
        }
        if !overwrite && item.dst.exists() {
            return false; // 目标已存在且不允许覆盖 → 必然跳过
        }
        true
    }

    /// 冲突分组的人类可读摘要（表格行：目标 + 冲突的源文件）。
    pub fn collision_rows(&self, src_root: &Path, dst_root: &Path) -> Vec<(String, String)> {
        self.collisions
            .iter()
            .map(|c| {
                (
                    display_path(&c.dst, dst_root),
                    c.sources
                        .iter()
                        .map(|s| display_path(s, src_root))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            })
            .collect()
    }
}

/// 目标被占用时生成 `name_1.ext` / `name_2.ext`… 直到唯一。
///
/// `reserved` 存 norm_key 后的字符串：大小写不敏感文件系统上 `IMG_1.NEF` 与
/// `img_1.nef` 是同一个文件，用原始字符串比较会让自动改名静默失效。
fn unique_target(
    dst: &Path,
    reserved: &mut std::collections::HashSet<String>,
    disk_occupied: bool,
) -> PathBuf {
    let key = norm_key(dst);
    if !reserved.contains(&key) && !disk_occupied {
        reserved.insert(key);
        return dst.to_path_buf();
    }
    let parent = dst.parent().unwrap_or_else(|| Path::new(""));
    let stem = dst
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let suffix = dst
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut n = 1;
    loop {
        let candidate = parent.join(format!("{stem}_{n}{suffix}"));
        let ckey = norm_key(&candidate);
        if !reserved.contains(&ckey) && !candidate.exists() {
            reserved.insert(ckey);
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_for(files: &[&str], strategy: Strategy) -> Plan {
        let src = Path::new("/src");
        let dst = Path::new("/dst");
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        let mut warnings = Vec::new();
        let raw = Plan::raw(
            &files,
            strategy,
            src,
            dst,
            "%Y/%m/%d",
            crate::model::DateSource::Created,
            &mut warnings,
        );
        Plan::build(raw, false, false, true)
    }

    #[test]
    fn flat_plan_detects_collision() {
        let plan = plan_for(
            &[
                "/src/DCIM/a/IMG_1.JPG",
                "/src/DCIM/b/IMG_1.JPG",
                "/src/DCIM/b/IMG_2.JPG",
            ],
            Strategy::Flat,
        );
        assert_eq!(plan.collisions.len(), 1);
        assert_eq!(plan.collisions[0].sources.len(), 2);
        // 没开自动改名：后到的被登记为重复
        assert!(plan
            .duplicates
            .contains_key(Path::new("/src/DCIM/b/IMG_1.JPG")));
        assert_eq!(plan.items.len(), 3);
    }

    #[test]
    fn auto_rename_makes_unique_targets() {
        let src = Path::new("/src");
        let dst = Path::new("/dst");
        let files = vec![
            PathBuf::from("/src/a/IMG_1.JPG"),
            PathBuf::from("/src/b/IMG_1.JPG"),
            PathBuf::from("/src/c/IMG_1.JPG"),
        ];
        let mut warnings = Vec::new();
        let raw = Plan::raw(
            &files,
            Strategy::Flat,
            src,
            dst,
            "%Y/%m/%d",
            crate::model::DateSource::Created,
            &mut warnings,
        );
        let plan = Plan::build(raw, true, false, true);
        let names: Vec<String> = plan
            .items
            .iter()
            .map(|i| i.dst.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["IMG_1.JPG", "IMG_1_1.JPG", "IMG_1_2.JPG"]);
        assert!(plan.duplicates.is_empty());
        // 冲突仍按 raw plan 统计，如实报告「检测到 1 组」
        assert_eq!(plan.collisions.len(), 1);
    }

    #[test]
    fn tree_strategy_keeps_structure() {
        let plan = plan_for(&["/src/DCIM/100CANON/IMG_1.JPG"], Strategy::Tree);
        assert_eq!(
            plan.items[0].dst,
            PathBuf::from("/dst/DCIM/100CANON/IMG_1.JPG")
        );
    }
}
