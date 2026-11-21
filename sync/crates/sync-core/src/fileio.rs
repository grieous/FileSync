//! 文件落地与读取。
//!
//! 两条硬规则：
//!   1. **原子落地**：先写同目录下的临时文件，校验通过后再 rename 覆盖。
//!      中途失败断电，目标文件仍是旧的完整版本，不会出现半个文件。
//!   2. **路径必须校验**：对端发来的相对路径可能是 `..\..\Windows\System32`，
//!      必须拒绝任何绝对路径、盘符、`..` 分量，否则等于给对方一个写任意文件的权限。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use sync_proto::CHUNK_SIZE;

/// 校验并转换相对路径。拒绝绝对路径、盘符、`..`、前缀组件。
pub fn safe_rel(rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        bail!("空路径");
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        bail!("拒绝绝对路径: {rel}");
    }
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => bail!("拒绝非法路径分量: {rel}"),
        }
    }
    Ok(p.to_path_buf())
}

/// 把相对路径拼到根目录下，保证结果仍在根目录内。
pub fn join_root(root: &Path, rel: &str) -> Result<PathBuf> {
    let safe = safe_rel(rel)?;
    let full = root.join(&safe);
    // 二次保险：规范化后必须仍以 root 为前缀
    if let (Ok(r), Ok(f)) = (root.canonicalize(), full.canonicalize()) {
        if !f.starts_with(&r) {
            bail!("路径越界: {rel}");
        }
    }
    Ok(full)
}

pub fn chunk_count(size: u64) -> u32 {
    let cs = CHUNK_SIZE as u64;
    size.div_ceil(cs) as u32
}

/// 读取指定分块。最后一块可能不足 CHUNK_SIZE。
pub fn read_chunk(path: &Path, idx: u32) -> Result<Vec<u8>> {
    let mut f = File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    f.seek(SeekFrom::Start(idx as u64 * CHUNK_SIZE as u64))?;

    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

/// 冲突副本路径：`名字.conflict-设备名-20261119-214500.ext`
pub fn conflict_path(rel: &str, device_short: &str, stamp: &str) -> String {
    let p = Path::new(rel);
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string());
    let ext = p.extension().map(|s| s.to_string_lossy().to_string());
    let parent = p.parent().map(|s| s.to_string_lossy().replace('\\', "/"));

    let base = match (stem, ext) {
        (Some(s), Some(e)) => format!("{s}.conflict-{device_short}-{stamp}.{e}"),
        (Some(s), None) => format!("{s}.conflict-{device_short}-{stamp}"),
        _ => format!("conflict-{device_short}-{stamp}"),
    };

    match parent {
        Some(d) if !d.is_empty() => format!("{d}/{base}"),
        _ => base,
    }
}

/// 正在接收的文件。边写边算哈希，收完再校验。
pub struct IncomingFile {
    tmp: PathBuf,
    target: PathBuf,
    file: File,
    hasher: blake3::Hasher,
    written: u64,
}

impl IncomingFile {
    pub fn create(root: &Path, rel: &str) -> Result<Self> {
        let target = join_root(root, rel)?;
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("创建目录 {}", dir.display()))?;
        }

        // 临时文件放同目录，保证 rename 是同分区操作（跨分区 rename 会退化成拷贝）
        let name = target
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let tmp = target.with_file_name(format!(".{name}.synctmp"));

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("创建临时文件 {}", tmp.display()))?;

        Ok(Self {
            tmp,
            target,
            file,
            hasher: blake3::Hasher::new(),
            written: 0,
        })
    }

    pub fn write_chunk(&mut self, data: &[u8]) -> Result<()> {
        self.file.write_all(data)?;
        self.hasher.update(data);
        self.written += data.len() as u64;
        Ok(())
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// 校验哈希后原子替换目标文件。校验失败会删掉临时文件并返回错误。
    pub fn finish(mut self, expect_hash: &str) -> Result<()> {
        self.file.flush()?;
        self.file.sync_all().ok();
        drop(self.file);

        let actual = self.hasher.finalize().to_hex().to_string();
        if actual != expect_hash {
            let _ = std::fs::remove_file(&self.tmp);
            bail!("哈希不匹配：期望 {expect_hash}，实际 {actual}");
        }

        // Windows 上 std::fs::rename 会替换已存在的目标（MoveFileEx + REPLACE_EXISTING）
        std::fs::rename(&self.tmp, &self.target)
            .with_context(|| format!("重命名到 {}", self.target.display()))?;
        Ok(())
    }

    /// 放弃本次接收，清理临时文件。
    pub fn abort(self) {
        let _ = std::fs::remove_file(&self.tmp);
    }
}

/// 删除文件和空目录。文件不存在视为成功（幂等）。
pub fn remove_path(root: &Path, rel: &str) -> Result<()> {
    let full = join_root(root, rel)?;
    if full.is_dir() {
        // 只删空目录，非空说明对端删除与本地新增冲突，保守起见保留
        let _ = std::fs::remove_dir(&full);
    } else if full.exists() {
        std::fs::remove_file(&full).with_context(|| format!("删除 {}", full.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal() {
        assert!(safe_rel("../etc/passwd").is_err());
        assert!(safe_rel("a/../../b").is_err());
        assert!(safe_rel("/abs/path").is_err());
        assert!(safe_rel("C:\\Windows\\x").is_err());
        assert!(safe_rel("a/b/c.txt").is_ok());
    }

    #[test]
    fn conflict_name_keeps_extension() {
        let n = conflict_path("docs/report.docx", "abc123", "20261119-214500");
        assert_eq!(n, "docs/report.conflict-abc123-20261119-214500.docx");
    }

    #[test]
    fn chunk_count_math() {
        assert_eq!(chunk_count(0), 0);
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(CHUNK_SIZE as u64), 1);
        assert_eq!(chunk_count(CHUNK_SIZE as u64 + 1), 2);
    }
}
