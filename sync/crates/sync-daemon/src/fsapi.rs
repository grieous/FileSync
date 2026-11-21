//! `/api/fs/list`：在**任务根目录内**列子目录。
//!
//! 浏览器拿不到本机绝对路径，所以「浏览…」这个动作改由 daemon 代跑。
//! 代价是必须自己防穿越：只认相对路径、拒绝 `..`、canonicalize 之后还要再比一次前缀。
//! 任何一步不满足就报错，不做「尽力而为」的猜测。

use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize)]
pub struct DirEntryView {
    pub name: String,
    /// 相对任务根目录的路径，用 `/` 分隔（Windows 上也不反斜杠，前端只做展示）。
    pub path: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ListView {
    pub dirs: Vec<DirEntryView>,
    pub parent: Option<String>,
    /// 当前目录相对根的位置（空串代表根）。
    pub path: String,
}

/// 只允许普通相对路径：不能绝对路径、不能 `..`、不能盘符。
fn clean_rel(rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim().replace('\\', "/");
    if rel.is_empty() || rel == "/" {
        return Ok(PathBuf::new());
    }
    let trimmed = rel.trim_start_matches('/');
    let mut out = PathBuf::new();
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err("路径里不能包含 ..".to_string()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("只接受相对任务根目录的路径".to_string())
            }
        }
    }
    Ok(out)
}

pub fn list(root: &str, rel: &str) -> Result<ListView, String> {
    let base = PathBuf::from(root.trim());
    if !base.is_dir() {
        return Err(format!("同步目录不存在：{}", base.display()));
    }
    let base_canon = base
        .canonicalize()
        .map_err(|e| format!("无法解析同步目录：{e}"))?;

    let rel_path = clean_rel(rel)?;
    let candidate = if rel_path.as_os_str().is_empty() {
        base.clone()
    } else {
        base.join(&rel_path)
    };
    let canon = candidate
        .canonicalize()
        .map_err(|_| "目录不存在".to_string())?;
    if !canon.starts_with(&base_canon) {
        return Err("路径超出同步目录".to_string());
    }
    if !canon.is_dir() {
        return Err("不是目录".to_string());
    }

    let mut dirs = Vec::new();
    let entries = std::fs::read_dir(&canon).map_err(|e| format!("读取目录失败：{e}"))?;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let child = if rel_path.as_os_str().is_empty() {
            name.clone()
        } else {
            format!("{}/{}", rel_path.to_string_lossy().replace('\\', "/"), name)
        };
        dirs.push(DirEntryView { name, path: child });
    }
    dirs.sort_by_key(|d| d.name.to_lowercase());

    let parent = if rel_path.as_os_str().is_empty() {
        None
    } else {
        let s = rel_path.to_string_lossy().replace('\\', "/");
        Some(match s.rsplit_once('/') {
            Some((head, _)) => head.to_string(),
            None => String::new(),
        })
    };

    Ok(ListView {
        dirs,
        parent,
        path: rel_path.to_string_lossy().replace('\\', "/"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        assert!(clean_rel("../etc").is_err());
        assert!(clean_rel("a/../../b").is_err());
        assert!(clean_rel("C:/Windows").is_err());
        assert!(clean_rel("/etc").is_ok(), "前导斜杠按「从根开始」处理");
        assert_eq!(clean_rel("a/b").unwrap(), PathBuf::from("a").join("b"));
    }

    #[test]
    fn lists_only_subdirectories_and_reports_parent() {
        let tmp = std::env::temp_dir().join(format!("ws-fsapi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Docs")).unwrap();
        std::fs::create_dir_all(tmp.join("Pics")).unwrap();
        std::fs::write(tmp.join("file.txt"), b"x").unwrap();

        let root = tmp.to_string_lossy().to_string();
        let view = list(&root, "").unwrap();
        let names: Vec<_> = view.dirs.iter().map(|d| d.name.clone()).collect();
        assert_eq!(names, vec!["Docs", "Pics"]);
        assert!(view.parent.is_none());

        let child = list(&root, "Docs").unwrap();
        assert_eq!(child.parent.as_deref(), Some(""));
        assert_eq!(child.path, "Docs");

        assert!(list(&root, "../").is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
