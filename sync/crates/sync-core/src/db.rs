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

-- 哈希缓存：(路径, 大小, 纳秒级 mtime) 未变则复用上次的 BLAKE3，不必重读文件。
-- 没有这张表时，每 30 秒的轮询都要把整个同步目录读一遍。
CREATE TABLE IF NOT EXISTS hash_cache (
    path     TEXT PRIMARY KEY,
    size     INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    blake3   TEXT NOT NULL
);

-- 文件级操作记录：界面上"记录"页直接读这张表。
-- 只留最近若干条（默认 2000），不当作审计日志长期保存。
CREATE TABLE IF NOT EXISTS activity (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    path       TEXT NOT NULL,
    action     TEXT NOT NULL,
    ok         INTEGER NOT NULL,
    message    TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_activity_time ON activity(created_at DESC);
"#;

/// 哈希缓存条目。大小与纳秒级 mtime 都没变，就认为内容没变，复用上次的哈希。
#[derive(Debug, Clone)]
pub struct CachedHash {
    pub size: u64,
    pub mtime_ns: i64,
    pub blake3: String,
}

/// 一条文件级记录，对应界面「记录」里的一行。
#[derive(Debug, Clone)]
pub struct Activity {
    pub path: String,
    /// 上传 / 下载 / 删除 / 冲突副本 / 建目录
    pub action: String,
    pub ok: bool,
    pub message: String,
    /// Unix 秒
    pub created_at: i64,
}

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

    /// 读出哈希缓存。键是相对路径。
    pub fn load_hash_cache(&self) -> Result<HashMap<String, CachedHash>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, size, mtime_ns, blake3 FROM hash_cache")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                CachedHash {
                    size: r.get::<_, i64>(1)? as u64,
                    mtime_ns: r.get(2)?,
                    blake3: r.get(3)?,
                },
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (k, v) = row?;
            out.insert(k, v);
        }
        Ok(out)
    }

    /// 整表替换。扫描结果就是当前真实状态，多余条目应丢弃。
    pub fn replace_hash_cache(&self, items: &HashMap<String, CachedHash>) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM hash_cache", [])?;
        {
            let mut stmt =
                tx.prepare("INSERT INTO hash_cache(path, size, mtime_ns, blake3) VALUES(?1,?2,?3,?4)")?;
            for (path, c) in items {
                stmt.execute(params![path, c.size as i64, c.mtime_ns, c.blake3])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 批量写入文件级记录。一次会话写一次，比逐条提交快得多。
    pub fn push_activities(&self, items: &[Activity]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO activity(path, action, ok, message, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
            )?;
            for a in items {
                stmt.execute(params![
                    a.path,
                    a.action,
                    a.ok as i64,
                    a.message,
                    a.created_at
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 最近的记录，按时间倒序。
    pub fn recent_activities(&self, limit: usize) -> Result<Vec<Activity>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, action, ok, message, created_at
             FROM activity ORDER BY created_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(Activity {
                path: r.get(0)?,
                action: r.get(1)?,
                ok: r.get::<_, i64>(2)? != 0,
                message: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn clear_activities(&self) -> Result<()> {
        self.conn.execute("DELETE FROM activity", [])?;
        Ok(())
    }

    /// 只保留最近 `keep` 条，避免状态库无限增长。
    pub fn prune_activities(&self, keep: usize) -> Result<()> {
        self.conn.execute(
            "DELETE FROM activity WHERE id NOT IN
             (SELECT id FROM activity ORDER BY id DESC LIMIT ?1)",
            [keep as i64],
        )?;
        Ok(())
    }

    /// 删除早于 `before`（Unix 秒）的记录。
    pub fn prune_activities_before(&self, before: i64) -> Result<usize> {
        let n = self
            .conn
            .execute("DELETE FROM activity WHERE created_at < ?1", [before])?;
        Ok(n)
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
