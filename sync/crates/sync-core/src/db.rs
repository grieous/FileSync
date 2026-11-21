//! 本地状态库。
//!
//! 只持久化三样东西：
//!   - `base`   : 上次同步成功时的共识快照（path -> blake3）
//!   - `pending`: 对端离线期间积压的本地变更，等对端上线再推
//!   - `meta`   : device_id / last_session_id / server_url 等
//!
//! 文件内容不在这里另存一份，永远只存在于用户自己的目录中。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sync_proto::{IndexEntry, Kind};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;

CREATE TABLE IF NOT EXISTS base (
    path    TEXT PRIMARY KEY,
    kind    INTEGER NOT NULL,
    size    INTEGER NOT NULL,
    blake3  TEXT NOT NULL,       -- 目录为空串
    mtime   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS pending (
    path    TEXT PRIMARY KEY,
    op      TEXT NOT NULL,       -- create|modify|delete
    blake3  TEXT,
    size    INTEGER,
    mtime   INTEGER,
    retry   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS meta (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
"#;

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).ok();
            }
        }
        let conn = Connection::open(path).with_context(|| format!("打开 {}", path.display()))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn meta(&self, k: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, k: &str, v: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(k, v) VALUES(?1, ?2)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![k, v],
        )?;
        Ok(())
    }

    /// 第一次运行生成并持久化，之后永远不变。服务端靠它区分两台设备。
    pub fn device_id(&self) -> Result<String> {
        if let Some(v) = self.meta("device_id")? {
            return Ok(v);
        }
        let v = uuid::Uuid::new_v4().to_string();
        self.set_meta("device_id", &v)?;
        Ok(v)
    }

    pub fn load_base(&self) -> Result<HashMap<String, IndexEntry>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, kind, size, blake3, mtime FROM base")?;
        let rows = stmt.query_map([], |r| {
            Ok(IndexEntry {
                path: r.get(0)?,
                kind: Kind::from_u8(r.get::<_, i64>(1)?),
                size: r.get::<_, i64>(2)? as u64,
                blake3: r.get(3)?,
                mtime: r.get(4)?,
            })
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let e = row?;
            out.insert(e.path.clone(), e);
        }
        Ok(out)
    }

    pub fn base_count(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM base", [], |r| r.get(0))?)
    }

    pub fn upsert_base(&self, e: &IndexEntry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO base(path, kind, size, blake3, mtime) VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                 kind = excluded.kind,
                 size = excluded.size,
                 blake3 = excluded.blake3,
                 mtime = excluded.mtime",
            params![
                e.path,
                e.kind.as_u8() as i64,
                e.size as i64,
                e.blake3,
                e.mtime
            ],
        )?;
        Ok(())
    }

    pub fn delete_base(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM base WHERE path = ?1", [path])?;
        Ok(())
    }

    /// 同步目录变更时必须调用：旧快照的 path 相对新目录已经无意义。
    pub fn clear_base(&self) -> Result<()> {
        let n = self.conn.execute("DELETE FROM base", [])?;
        if n > 0 {
            tracing::warn!("已清空 base 快照（{n} 条），下次会话将重建共识");
        }
        Ok(())
    }

    /// 目录变更时同样要丢弃，否则会针对已不存在的路径反复重试。
    pub fn clear_pending(&self) -> Result<()> {
        self.conn.execute("DELETE FROM pending", [])?;
        Ok(())
    }

    /// 同步根目录。持久化在 meta 里，使得 --root 只需首次指定。
    pub fn root_path(&self) -> Result<Option<String>> {
        self.meta("root")
    }

    pub fn set_root_path(&self, p: &str) -> Result<()> {
        self.set_meta("root", p)
    }

    /// 会话失败时清空 base 的重建路径：整表替换。
    pub fn replace_base(&mut self, entries: &[IndexEntry]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM base", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO base(path, kind, size, blake3, mtime) VALUES(?1, ?2, ?3, ?4, ?5)",
            )?;
            for e in entries {
                stmt.execute(params![
                    e.path,
                    e.kind.as_u8() as i64,
                    e.size as i64,
                    e.blake3,
                    e.mtime
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}
