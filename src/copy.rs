//! 单文件复制：原子写入、可选校验和、跳过原因说明。
//!
//! 安全底线：**绝不先删除已存在的目标文件**。
//! 先写目标同目录下的临时文件，全部成功（含可选校验）后再原子替换；
//! 任何一步出错都只删临时文件，原目标文件保持完好。原 Python 脚本早期版本
//! 先 `dst.unlink()` 再写，写失败就同时丢了旧照片。

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use digest::DynDigest;

use crate::config::Config;
use crate::model::{display_path, format_size, FileAction, DEFAULT_COPY_BUFSIZE};

/// 临时文件名计数器：并行线程各写各的临时文件，互不冲突。
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 一次单文件复制的结果。
#[derive(Debug, Clone)]
pub struct CopyOutcome {
    pub action: FileAction,
    pub note: String,
    /// 实际写入的字节（只有 Copied/Overwritten 才有意义）。
    pub bytes: u64,
}

/// 复制的错误类型：区分「可预期失败」与「中断」。
#[derive(Debug)]
pub enum CopyError {
    /// 普通失败（IO、校验不匹配等），记一条失败记录继续跑。
    Failed(String),
    /// 收到中断信号，必须立刻停止。
    Interrupted,
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CopyError::Failed(msg) => write!(f, "{msg}"),
            CopyError::Interrupted => write!(f, "已中断"),
        }
    }
}

/// 目标已存在被跳过时，说明「为什么跳过可能是错的」。
///
/// 目标同名但内容不同（大小不一致）是最容易丢照片的场景：相机计数器归零，
/// 两张卡都有 `IMG_0001.NEF`，先复制的那张占住位置，后一张被静默跳过。
pub fn explain_skip(src: &Path, dst: &Path) -> String {
    let (Ok(s), Ok(d)) = (std::fs::metadata(src), std::fs::metadata(dst)) else {
        return String::new();
    };
    if s.len() != d.len() {
        return format!(
            "目标已存在但大小不同（源 {}，目标 {}），未覆盖，内容可能不同",
            format_size(s.len()),
            format_size(d.len())
        );
    }
    String::new()
}

/// 按名字构造摘要器。名字已在 config 层校验过，这里仍返回 Option 以防直调。
pub fn new_hasher(algo: &str) -> Option<Box<dyn DynDigest>> {
    match algo {
        "md5" => Some(Box::new(md5::Md5::default())),
        "sha1" => Some(Box::new(sha1::Sha1::default())),
        "sha256" => Some(Box::new(sha2::Sha256::default())),
        "sha512" => Some(Box::new(sha2::Sha512::default())),
        "blake2b" => Some(Box::new(blake2::Blake2b512::default())),
        _ => None,
    }
}

/// 流式计算文件摘要，返回十六进制字符串。
pub fn file_digest(path: &Path, algo: &str) -> io::Result<String> {
    let mut hasher = new_hasher(algo)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("未知算法 {algo}")))?;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// 复制一个文件，返回动作与备注。计数器由调用方根据结果累加。
pub fn copy_one(
    config: &Config,
    src: &Path,
    dst: &Path,
    duplicate_of: Option<&Path>,
    src_root: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<CopyOutcome, CopyError> {
    // 同一目标被本次计划里的另一个源文件先占用：明确按跳过处理
    if let Some(winner) = duplicate_of {
        return Ok(CopyOutcome {
            action: FileAction::Skipped,
            note: format!("与 {} 映射到同一目标", display_path(winner, src_root)),
            bytes: 0,
        });
    }

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CopyError::Failed(format!("创建目标目录失败: {}: {e}", parent.display()))
        })?;
    }

    let dst_exists = dst.exists();
    if dst_exists && dst.is_dir() {
        return Err(CopyError::Failed("目标路径已存在且为目录".to_string()));
    }
    let action = if dst_exists {
        if !config.overwrite {
            return Ok(CopyOutcome {
                action: FileAction::Skipped,
                note: explain_skip(src, dst),
                bytes: 0,
            });
        }
        FileAction::Overwritten
    } else {
        FileAction::Copied
    };

    if same_file(src, dst) {
        return Err(CopyError::Failed("源和目标为同一文件".to_string()));
    }

    let tmp = create_temp(dst).map_err(|e| CopyError::Failed(format!("创建临时文件失败: {e}")))?;
    // 一切失败路径都走这里：只删临时文件，目标文件从未被动过
    let result = (|| -> Result<CopyOutcome, CopyError> {
        copy_contents(src, &tmp, config.buffer_size, cancelled)?;
        if cancelled() {
            return Err(CopyError::Interrupted);
        }
        let mut note = String::new();
        if config.verify {
            let algo = config.verify_algorithm.as_str();
            let src_hash = file_digest(src, algo)
                .map_err(|e| CopyError::Failed(format!("计算源文件摘要失败: {e}")))?;
            let tmp_hash = file_digest(&tmp, algo)
                .map_err(|e| CopyError::Failed(format!("计算副本摘要失败: {e}")))?;
            if src_hash != tmp_hash {
                let short = |s: &str| s.chars().take(12).collect::<String>();
                return Err(CopyError::Failed(format!(
                    "{algo} 校验和不匹配（源 {}… / 副本 {}…），已放弃替换",
                    short(&src_hash),
                    short(&tmp_hash)
                )));
            }
            note = format!("{algo} 校验通过");
        }
        // 信号已到达时绝不能再落地目标文件，否则退出后残留「成功」的假象
        if cancelled() {
            return Err(CopyError::Interrupted);
        }
        replace_file(&tmp, dst).map_err(|e| CopyError::Failed(format!("替换目标文件失败: {e}")))?;
        let bytes = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        Ok(CopyOutcome {
            action,
            note,
            bytes,
        })
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn same_file(src: &Path, dst: &Path) -> bool {
    match (src.canonicalize(), dst.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 在目标同目录创建唯一的临时文件（同分区才能保证 rename 原子）。
fn create_temp(dst: &Path) -> io::Result<PathBuf> {
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    let pid = std::process::id();
    loop {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let candidate = parent.join(format!(".filecopy_{pid}_{seq}_{nanos}.tmp"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// 复制内容并保留权限/时间戳。
///
/// 缓冲区为默认值时交给 `std::fs::copy`（macOS 上会走 clonefile/copyfile，
/// 比手写循环快得多）；用户显式指定缓冲区时才走手写循环，并定期检查中断。
fn copy_contents(
    src: &Path,
    dst: &Path,
    buffer_size: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), CopyError> {
    let manual = buffer_size != 0 && buffer_size != DEFAULT_COPY_BUFSIZE;
    if manual {
        let mut reader = std::fs::File::open(src)
            .map_err(|e| CopyError::Failed(format!("打开源文件失败: {e}")))?;
        let mut writer = std::fs::File::create(dst)
            .map_err(|e| CopyError::Failed(format!("创建目标文件失败: {e}")))?;
        let chunk = buffer_size.clamp(4096, 64 * 1024 * 1024) as usize;
        let mut buf = vec![0u8; chunk];
        let mut written: u64 = 0;
        loop {
            if cancelled() {
                return Err(CopyError::Interrupted);
            }
            let n = reader
                .read(&mut buf)
                .map_err(|e| CopyError::Failed(format!("读取源文件失败: {e}")))?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .map_err(|e| CopyError::Failed(format!("写目标文件失败: {e}")))?;
            written += n as u64;
        }
        writer
            .flush()
            .map_err(|e| CopyError::Failed(format!("刷新目标文件失败: {e}")))?;
        let expected = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        if written != expected {
            // 读到的字节少于源文件大小说明源在复制过程中被改动/截断
            return Err(CopyError::Failed(format!(
                "复制长度不一致（应为 {} 字节，实际写入 {written} 字节）",
                expected
            )));
        }
    } else {
        std::fs::copy(src, dst).map_err(|e| CopyError::Failed(format!("复制失败: {e}")))?;
    }
    copy_metadata(src, dst);
    Ok(())
}

/// 尽力保留权限与时间戳；失败不影响复制结果（与 copy2 的 best-effort 一致）。
fn copy_metadata(src: &Path, dst: &Path) {
    if let Ok(meta) = std::fs::metadata(src) {
        let _ = std::fs::set_permissions(dst, meta.permissions());
        let atime = filetime::FileTime::from_last_access_time(&meta);
        let mtime = filetime::FileTime::from_last_modification_time(&meta);
        let _ = filetime::set_file_times(dst, atime, mtime);
    }
}

/// 原子替换；Windows 上 `rename` 不能覆盖已存在文件，退化成「先删再改名」。
fn replace_file(tmp: &Path, dst: &Path) -> io::Result<()> {
    match std::fs::rename(tmp, dst) {
        Ok(()) => Ok(()),
        Err(e) => {
            #[cfg(windows)]
            {
                if dst.exists() {
                    std::fs::remove_file(dst)?;
                    return std::fs::rename(tmp, dst);
                }
            }
            Err(e)
        }
    }
}

/// 判断目录是否可写：复制前先用一次，避免跑到一半才发现目标不可写。
pub fn is_writable_dir(dir: &Path) -> bool {
    let probe = dir.join(format!(".filecopy_probe_{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 汇总用：把 (动作 → 数量) 累加成小表。
pub fn count_actions(actions: &[FileAction]) -> HashMap<FileAction, usize> {
    let mut map = HashMap::new();
    for a in actions {
        *map.entry(*a).or_insert(0) += 1;
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_known_values() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        assert_eq!(
            file_digest(&f, "md5").unwrap(),
            "5d41402abc4b2a76b9719d911017c592"
        );
        assert_eq!(
            file_digest(&f, "sha256").unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(
            file_digest(&f, "sha1").unwrap(),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        );
    }

    #[test]
    fn copy_writes_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jpg");
        let dst = dir.path().join("out/dst.jpg");
        std::fs::write(&src, b"data").unwrap();
        let mut cfg = Config {
            verify: true,
            ..Default::default()
        };
        cfg.verify_algorithm = "sha256".to_string();
        let out = copy_one(&cfg, &src, &dst, None, &src, &|| false).unwrap();
        assert_eq!(out.action, FileAction::Copied);
        assert_eq!(out.note, "sha256 校验通过");
        assert_eq!(std::fs::read(&dst).unwrap(), b"data");
    }

    #[test]
    fn skip_without_overwrite_keeps_original() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jpg");
        let dst = dir.path().join("dst.jpg");
        std::fs::write(&src, b"new-content").unwrap();
        std::fs::write(&dst, b"old").unwrap();
        let cfg = Config::default();
        let out = copy_one(&cfg, &src, &dst, None, &src, &|| false).unwrap();
        assert_eq!(out.action, FileAction::Skipped);
        assert!(out.note.contains("大小不同"), "{}", out.note);
        assert_eq!(std::fs::read(&dst).unwrap(), b"old");
    }

    #[test]
    fn overwrite_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jpg");
        let dst = dir.path().join("dst.jpg");
        std::fs::write(&src, b"new-content").unwrap();
        std::fs::write(&dst, b"old").unwrap();
        let cfg = Config {
            overwrite: true,
            ..Default::default()
        };
        let out = copy_one(&cfg, &src, &dst, None, &src, &|| false).unwrap();
        assert_eq!(out.action, FileAction::Overwritten);
        assert_eq!(std::fs::read(&dst).unwrap(), b"new-content");
    }

    #[test]
    fn failed_verify_keeps_target_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jpg");
        let dst = dir.path().join("dst.jpg");
        std::fs::write(&src, b"new-content").unwrap();
        std::fs::write(&dst, b"old").unwrap();
        let mut cfg = Config {
            overwrite: true,
            verify: true,
            ..Default::default()
        };
        cfg.verify_algorithm = "nope".to_string(); // 未知算法 → 计算失败
        let err = copy_one(&cfg, &src, &dst, None, &src, &|| false).unwrap_err();
        assert!(matches!(err, CopyError::Failed(_)));
        assert_eq!(std::fs::read(&dst).unwrap(), b"old");
        // 没有残留临时文件
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".filecopy_"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn manual_buffer_loop_copies() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &payload).unwrap();
        let cfg = Config {
            buffer_size: 8192,
            ..Default::default()
        };
        let out = copy_one(&cfg, &src, &dst, None, &src, &|| false).unwrap();
        assert_eq!(out.action, FileAction::Copied);
        assert_eq!(std::fs::read(&dst).unwrap(), payload);
    }
}
