//! 与 exe 的唯一交集：同一个目录不能被两边同时同步。
//!
//! 状态、配置、互斥体全都隔离，但文件系统隔离不了：两个进程同步同一个目录时，
//! 各自看到的「对端删除了某文件」都会成立，于是互相删。这不是理论风险——
//! 历史上真出过整目录被清空的事故。
//!
//! 所以这里**只读**地看一眼 exe 的配置，命中就拒绝启动该任务：
//! 不写、不加锁、读失败也不影响 exe 运行。读不到且 exe 正在跑时按「无法确认」处理，
//! 宁可拒绝也不赌。

use std::path::PathBuf;

use serde_json::Value;

/// exe 自己那棵树（`%APPDATA%\WinSync`），与 daemon 的 `WINSYNC_STATE_DIR` 无关。
fn exe_config_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("WinSync")
        .join("config.json")
}

pub struct ExeTask {
    pub name: String,
    pub root: String,
}

/// 读 exe 的任务列表；文件不存在返回 `Some(空)`，解析失败返回 `None`。
pub fn exe_tasks() -> Option<Vec<ExeTask>> {
    let path = exe_config_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start_matches('\u{feff}');
    let value: Value = serde_json::from_str(text).ok()?;

    // 新格式：{ tasks: [...] }；旧格式：单任务扁平对象
    if let Some(list) = value.get("tasks").and_then(|v| v.as_array()) {
        let tasks = list
            .iter()
            .filter_map(|t| {
                let root = t.get("root")?.as_str()?.trim().to_string();
                if root.is_empty() {
                    return None;
                }
                let name = t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| root.clone());
                Some(ExeTask { name, root })
            })
            .collect();
        return Some(tasks);
    }

    let root = value.get("root")?.as_str()?.trim().to_string();
    if root.is_empty() {
        Some(Vec::new())
    } else {
        Some(vec![ExeTask {
            name: root.clone(),
            root,
        }])
    }
}

/// 规范化：小写、统一反斜杠、去掉末尾分隔符（`D:\` 变成 `d:`，代表盘根）。
pub fn normalize(path: &str) -> String {
    let p = path.trim().replace('/', "\\");
    let mut out = p.to_lowercase();
    while out.len() > 2 && out.ends_with('\\') {
        out.pop();
    }
    out
}

fn is_child_of(child: &str, parent: &str) -> bool {
    if parent.ends_with(':') {
        return child.starts_with(&format!("{parent}\\"));
    }
    child
        .strip_prefix(parent)
        .map(|rest| rest.starts_with('\\'))
        .unwrap_or(false)
}

/// 两个目录是否重叠：相同、互为父子都算。
pub fn overlaps(a: &str, b: &str) -> bool {
    let (a, b) = (normalize(a), normalize(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || is_child_of(&a, &b) || is_child_of(&b, &a)
}

/// 启动前检查。`Ok(())` 表示可以同步；`Err(文案)` 直接给用户看。
pub fn conflict_for(root: &str) -> Result<(), String> {
    let Some(tasks) = exe_tasks() else {
        if crate::win::exe_running() {
            return Err(
                "WinSync.exe 正在运行，但读不到它的配置，无法确认同步目录是否重叠。\
                 请关闭 exe 后重试（或先在 exe 里确认没有指向同一目录的任务）"
                    .to_string(),
            );
        }
        return Ok(());
    };
    for task in tasks {
        if overlaps(root, &task.root) {
            return Err(format!(
                "同步目录「{root}」与 WinSync.exe 的任务「{}」重叠（{}）。\
                 两个程序同时同步同一目录会互相删文件，已拒绝启动。\
                 要在这个目录上用插件同步，请先在 exe 里删掉那个任务（暂停不算，\
                 配置还在就会被拦），或者给插件换一个目录。",
                task.name, task.root
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_path_with_different_case_and_slashes() {
        assert!(overlaps("D:\\Sync", "d:/sync/"));
    }

    #[test]
    fn parent_and_child_overlap_both_ways() {
        assert!(overlaps("D:\\Sync", "D:\\Sync\\Docs"));
        assert!(overlaps("D:\\Sync\\Docs", "D:\\Sync"));
    }

    #[test]
    fn siblings_do_not_overlap() {
        assert!(!overlaps("D:\\Sync", "D:\\Sync2"));
        assert!(!overlaps("D:\\A\\B", "D:\\A\\BC"));
    }

    #[test]
    fn drive_root_covers_everything_on_it() {
        assert!(overlaps("D:\\", "D:\\Sync"));
        assert!(!overlaps("D:\\", "E:\\Sync"));
    }

    #[test]
    fn empty_paths_never_conflict() {
        assert!(!overlaps("", "D:\\Sync"));
    }
}
