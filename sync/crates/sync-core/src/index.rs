//! 文件扫描、内容哈希、共识摘要，以及核心的三方差异计算。
//!
//! 差异计算的三个输入：
//!   base   = 上次同步成功时双方的状态（共识）
//!   local  = 本地磁盘当前状态
//!   remote = 本次会话拿到的对端索引
//!
//! 判定规则见 docs/windows-sync-design.md 第 3.1 节。

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use sync_proto::{IndexEntry, Kind};
use walkdir::WalkDir;

/// 把绝对路径转成协议用的相对路径（`/` 分隔）。
pub fn to_rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 计算文件内容的 BLAKE3（小写 hex），流式读取，不占内存。
pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// 递归扫描目录，产出索引。无法读取的条目跳过并告警，不中断整体扫描。
pub fn scan(root: &Path) -> Result<Vec<IndexEntry>> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut out = Vec::new();

    for item in WalkDir::new(&root).follow_links(false).sort_by_file_name() {
        let item = match item {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("跳过无法访问的条目: {e}");
                continue;
            }
        };
        let path = item.path();
        if path == root {
            continue;
        }
        let rel = to_rel(&root, path);
        if rel.is_empty() {
            continue;
        }
        let md = match item.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let is_dir = md.is_dir();
        let digest = if is_dir {
            String::new()
        } else {
            match hash_file(path) {
                Ok(d) => d,
                Err(e) => {
                    // 文件被独占打开（Office / 数据库）时走到这里，交给上层重试队列。
                    tracing::warn!("跳过 {rel}: {e}");
                    continue;
                }
            }
        };

        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        out.push(IndexEntry {
            path: rel,
            kind: if is_dir { Kind::Dir } else { Kind::File },
            size: if is_dir { 0 } else { md.len() },
            blake3: digest,
            mtime,
        });
    }

    Ok(out)
}

/// 把所有条目的 (path, blake3, size) 排序后做一次 BLAKE3。
/// 摘要相同 = 两端文件树完全一致，可以直接结束会话，一个字节都不用传。
pub fn root_hash(entries: &[IndexEntry]) -> String {
    let mut sorted: Vec<&IndexEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    let mut h = blake3::Hasher::new();
    for e in sorted {
        h.update(e.path.as_bytes());
        h.update(&[0]);
        h.update(e.blake3.as_bytes());
        h.update(&[0]);
        h.update(&e.size.to_be_bytes());
        h.update(&[0]);
    }
    h.finalize().to_hex().to_string()
}

#[derive(Debug, Default)]
pub struct Plan {
    /// 本地有、需要推给对端
    pub push: Vec<IndexEntry>,
    /// 对端有、需要拉到本地
    pub pull: Vec<IndexEntry>,
    /// 本地已删，需要通知对端删
    pub delete_remote: Vec<String>,
    /// 对端已删，需要本地删
    pub delete_local: Vec<String>,
    /// 双方都改了且内容不同：保留双份
    pub conflicts: Vec<IndexEntry>,
    /// 双方都已删，可以从 base 移除
    pub forget: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.push.is_empty()
            && self.pull.is_empty()
            && self.delete_remote.is_empty()
            && self.delete_local.is_empty()
            && self.conflicts.is_empty()
            && self.forget.is_empty()
    }
}

/// 三方比较。`base` 缺失等价于"没有共识"，此时任何双方共存的差异都算冲突。
pub fn diff(
    base: &HashMap<String, IndexEntry>,
    local: &HashMap<String, IndexEntry>,
    remote: &HashMap<String, IndexEntry>,
) -> Plan {
    let mut plan = Plan::default();

    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for k in base.keys() {
        keys.insert(k.as_str());
    }
    for k in local.keys() {
        keys.insert(k.as_str());
    }
    for k in remote.keys() {
        keys.insert(k.as_str());
    }

    // 与共识相比是否发生了变化。目录没有内容哈希，只比类型。
    fn changed(x: &IndexEntry, b: &IndexEntry) -> bool {
        x.kind != b.kind || x.size != b.size || x.blake3 != b.blake3
    }

    for k in keys {
        let b = base.get(k);
        let l = local.get(k);
        let r = remote.get(k);

        match (b, l, r) {
            // 双方都已不存在 → 清理共识
            (Some(_), None, None) => plan.forget.push(k.to_string()),

            // 本地删除 vs 对端状态
            (Some(bb), None, Some(rr)) => {
                if changed(rr, bb) {
                    // 对端改过 → 修改胜出，local 重新拉回
                    plan.pull.push(rr.clone());
                } else {
                    plan.delete_remote.push(k.to_string());
                }
            }

            // 对端删除 vs 本地状态
            (Some(bb), Some(ll), None) => {
                if changed(ll, bb) {
                    // 本地改过 → 修改胜出，推回对端
                    plan.push.push(ll.clone());
                } else {
                    plan.delete_local.push(k.to_string());
                }
            }

            // 三方都在
            (Some(bb), Some(ll), Some(rr)) => {
                let lc = changed(ll, bb);
                let rc = changed(rr, bb);
                match (lc, rc) {
                    (false, false) => {}
                    (false, true) => plan.pull.push(rr.clone()),
                    (true, false) => plan.push.push(ll.clone()),
                    (true, true) => {
                        if ll.blake3 == rr.blake3 {
                            // 殊途同归，内容一致，不算冲突
                        } else {
                            plan.conflicts.push(ll.clone());
                            plan.pull.push(rr.clone());
                        }
                    }
                }
            }

            // 本地新增
            (None, Some(ll), None) => plan.push.push(ll.clone()),
            // 对端新增
            (None, None, Some(rr)) => plan.pull.push(rr.clone()),
            // 双方都新增
            (None, Some(ll), Some(rr)) => {
                if ll.blake3 != rr.blake3 {
                    plan.conflicts.push(ll.clone());
                    plan.pull.push(rr.clone());
                }
            }

            (None, None, None) => {}
        }
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str, digest: &str) -> IndexEntry {
        IndexEntry {
            path: path.into(),
            kind: Kind::File,
            size: 1,
            blake3: digest.into(),
            mtime: 0,
        }
    }

    fn map(items: Vec<IndexEntry>) -> HashMap<String, IndexEntry> {
        items.into_iter().map(|e| (e.path.clone(), e)).collect()
    }

    #[test]
    fn local_change_pushes() {
        let base = map(vec![f("a", "1")]);
        let local = map(vec![f("a", "2")]);
        let remote = map(vec![f("a", "1")]);
        let p = diff(&base, &local, &remote);
        assert_eq!(p.push.len(), 1);
        assert!(p.pull.is_empty());
        assert!(p.conflicts.is_empty());
    }

    #[test]
    fn both_changed_conflicts() {
        let base = map(vec![f("a", "1")]);
        let local = map(vec![f("a", "2")]);
        let remote = map(vec![f("a", "3")]);
        let p = diff(&base, &local, &remote);
        assert_eq!(p.conflicts.len(), 1);
        assert_eq!(p.pull.len(), 1);
    }

    #[test]
    fn delete_vs_modify_keeps_modification() {
        let base = map(vec![f("a", "1")]);
        let local = map(vec![]); // 本地删了
        let remote = map(vec![f("a", "2")]); // 对端改了
        let p = diff(&base, &local, &remote);
        assert_eq!(p.pull.len(), 1);
        assert!(p.delete_remote.is_empty());
    }

    #[test]
    fn delete_propagates_when_peer_unchanged() {
        let base = map(vec![f("a", "1")]);
        let local = map(vec![]);
        let remote = map(vec![f("a", "1")]);
        let p = diff(&base, &local, &remote);
        assert_eq!(p.delete_remote, vec!["a".to_string()]);
    }

    #[test]
    fn identical_tree_is_empty_plan() {
        let base = map(vec![f("a", "1")]);
        let local = map(vec![f("a", "1")]);
        let remote = map(vec![f("a", "1")]);
        assert!(diff(&base, &local, &remote).is_empty());
    }
}
