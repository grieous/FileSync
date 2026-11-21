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
use serde::{Deserialize, Serialize};
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

/// 扫描时的过滤规则。语义与同类同步工具一致：
/// 被过滤的路径在两端都"不存在"，因此永远不会参与差异计算。
///
/// 重要：**两台设备必须配置同一套规则**。否则一方忽略的文件，在另一方看来
/// 就是"被删除了"，会把删除操作传播回去。会话开始时会比对签名并拒绝不一致的组合。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    /// 忽略 `.` 开头的文件和目录（`.git`、`.DS_Store` 等）
    #[serde(default)]
    pub ignore_dotfiles: bool,
    /// 忽略的扩展名，不带点，大小写不敏感
    #[serde(default)]
    pub ignore_extensions: Vec<String>,
    /// 小于该大小的文件跳过（字节）
    #[serde(default)]
    pub size_min: Option<u64>,
    /// 大于该大小的文件跳过（字节）
    #[serde(default)]
    pub size_max: Option<u64>,
}

impl Default for Filter {
    fn default() -> Self {
        // 默认不过滤任何东西。开启过滤会让已有共识失效，
        // 必须由用户显式选择，不能升级后悄悄生效。
        Self {
            ignore_dotfiles: false,
            ignore_extensions: Vec::new(),
            size_min: None,
            size_max: None,
        }
    }
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        !self.ignore_dotfiles
            && self.ignore_extensions.is_empty()
            && self.size_min.is_none()
            && self.size_max.is_none()
    }

    /// 规范化签名，用于两端比对。扩展名排序去重，保证等价配置得到相同字符串。
    pub fn signature(&self) -> String {
        let mut ext: Vec<String> = self
            .ignore_extensions
            .iter()
            .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        ext.sort();
        ext.dedup();
        format!(
            "dot={};ext={};min={};max={}",
            self.ignore_dotfiles as u8,
            ext.join(","),
            self.size_min.map(|v| v.to_string()).unwrap_or_default(),
            self.size_max.map(|v| v.to_string()).unwrap_or_default()
        )
    }

    fn ext_matches(&self, name: &str) -> bool {
        let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str()) else {
            return false;
        };
        let ext = ext.to_ascii_lowercase();
        self.ignore_extensions
            .iter()
            .any(|e| e.trim().trim_start_matches('.').to_ascii_lowercase() == ext)
    }

    /// 相对路径中任一层以 `.` 开头就整枝跳过。
    fn dot_component(&self, rel: &str) -> bool {
        rel.split('/').any(|c| c.starts_with('.') && c != "." && c != "..")
    }
}

/// 递归扫描目录，产出索引。无法读取的条目跳过并告警，不中断整体扫描。
pub fn scan(root: &Path) -> Result<Vec<IndexEntry>> {
    scan_with(root, &Filter::default())
}

/// 带过滤规则的扫描。
pub fn scan_with(root: &Path, filter: &Filter) -> Result<Vec<IndexEntry>> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut out = Vec::new();
    let mut skipped = 0usize;

    for item in WalkDir::new(&root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        // 目录级剪枝：点开头的一整枝都不必遍历（.git 动辄几万文件）
        .filter_entry(|e| {
            if !filter.ignore_dotfiles {
                return true;
            }
            let p = e.path();
            if p == root {
                return true;
            }
            let rel = to_rel(&root, p);
            !filter.dot_component(&rel)
        })
    {
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

        if !is_dir {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            // 传输中途的临时文件永远不该进索引
            if name.ends_with(".synctmp") {
                skipped += 1;
                continue;
            }
            if filter.ext_matches(&name) {
                skipped += 1;
                continue;
            }
            let size = md.len();
            if filter.size_min.is_some_and(|v| size < v) || filter.size_max.is_some_and(|v| size > v)
            {
                skipped += 1;
                continue;
            }
        }

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

    if skipped > 0 {
        tracing::info!("按过滤规则跳过 {skipped} 个文件");
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

    // ---- 过滤规则 ----

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("winsync-filter-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn filter_skips_dotfiles_and_extensions() {
        let root = tmpdir("dot");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), b"x").unwrap();
        std::fs::write(root.join(".env"), b"secret").unwrap();
        std::fs::write(root.join("a.txt"), b"aaa").unwrap();
        std::fs::write(root.join("setup.LNK"), b"lnk").unwrap();

        let all = scan_with(&root, &Filter::default()).unwrap();
        let names: Vec<&str> = all.iter().map(|e| e.path.as_str()).collect();
        assert!(names.contains(&".env"));
        assert!(names.contains(&"setup.LNK"));

        let filter = Filter {
            ignore_dotfiles: true,
            ignore_extensions: vec!["lnk".into()],
            ..Default::default()
        };
        let kept = scan_with(&root, &filter).unwrap();
        let names: Vec<&str> = kept.iter().map(|e| e.path.as_str()).collect();
        assert!(!names.iter().any(|n| n.starts_with('.')));
        assert!(!names.contains(&"setup.LNK"));
        assert!(names.contains(&"a.txt"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn filter_respects_size_bounds() {
        let root = tmpdir("size");
        std::fs::write(root.join("small.bin"), vec![0u8; 10]).unwrap();
        std::fs::write(root.join("big.bin"), vec![0u8; 4096]).unwrap();

        let filter = Filter {
            size_min: Some(100),
            size_max: Some(2048),
            ..Default::default()
        };
        let kept = scan_with(&root, &filter).unwrap();
        assert!(kept.is_empty(), "两个文件都应被大小规则排除");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn signature_is_order_insensitive() {
        let a = Filter {
            ignore_dotfiles: true,
            ignore_extensions: vec!["LNK".into(), ".tmp".into()],
            ..Default::default()
        };
        let b = Filter {
            ignore_dotfiles: true,
            ignore_extensions: vec!["tmp".into(), "lnk".into()],
            ..Default::default()
        };
        assert_eq!(a.signature(), b.signature());
        assert_ne!(a.signature(), Filter::default().signature());
    }
}
