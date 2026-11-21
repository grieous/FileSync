//! 一次完整同步会话。
//!
//! 为什么是**单向串行**而不是双向并发：
//! 两端同时推会导致 Offer / Request / Chunk / Ack 四条消息流交错，状态机极难写对。
//! 因为只有两台设备，约定 device_id 字典序小的一方先推，另一方先收，完成后角色交换。
//! 代价是总耗时略长，但换来的是可验证的顺序语义——这笔交易值得。
//!
//! 会话顺序：
//!   1. Hello → PeerStatus（服务器告知对端是否在线）
//!   2. IndexDigest 比对摘要，相同则直接结束（零传输）
//!   3. 不同则互发 IndexChunk
//!   4. 三方 diff 算出计划
//!   5. initiator 先推自己的 push 列表 → 发 PushDone → 转入接收
//!      responder 先接收（直到收到 PushDone）→ 再推自己的 push 列表 → 发 PushDone
//!   6. 传播删除
//!   7. 重扫本地并写入新的 base 快照

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use crate::config::{Config, SyncMode};
use sync_core::{
    chunk_count, conflict_path, diff, join_root, read_chunk, remove_path, root_hash, scan_cached,
    Activity, IncomingFile, Store,
};
use sync_proto::{ChunkHeader, Frame, IndexEntry, Msg, INDEX_PAGE, PROTO_VER};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 会话过程中的事件，GUI 用它刷新界面，CLI 用它打印。
#[derive(Debug, Clone)]
pub enum Event {
    Log(String),
    Progress { done: u64, total: u64, what: String },
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub pushed: usize,
    pub pulled: usize,
    pub deleted: usize,
    pub conflicts: usize,
    pub bytes_sent: u64,
    pub bytes_recv: u64,
    /// 两端完全一致，本次没有传输任何内容
    pub up_to_date: bool,
    /// 本次会话的文件级记录，结束后写入状态库供界面展示
    pub activities: Vec<Activity>,
}

struct Session {
    sink: SplitSink<Ws, Message>,
    stream: SplitStream<Ws>,
    root: PathBuf,
    device_id: String,
    peer_id: String,
    tx: Option<Sender<Event>>,
    report: Report,
    quiet: bool,
    /// 等待对端上线的上限。GUI 自动同步会给很短的值，避免挂住。
    peer_timeout: std::time::Duration,
    /// 本次确认推给对端成功的路径
    pushed_ok: HashSet<String>,
    /// 本次从对端成功落地的路径
    pulled_ok: HashSet<String>,
}

impl Session {
    fn emit(&self, s: impl Into<String>) {
        let s = s.into();
        if let Some(tx) = &self.tx {
            let _ = tx.send(Event::Log(s.clone()));
        }
        if !self.quiet {
            println!("{s}");
        }
    }

    fn progress(&self, done: u64, total: u64, what: String) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Event::Progress { done, total, what });
        }
    }

    /// 记一条文件级记录。界面「记录」页就是这些行的汇总。
    fn note(&mut self, path: &str, action: &str, ok: bool, message: &str) {
        self.report.activities.push(Activity {
            path: path.to_string(),
            action: action.to_string(),
            ok,
            message: message.to_string(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        });
    }

    async fn send(&mut self, m: Msg) -> Result<()> {
        self.sink
            .send(Message::binary(Frame::Control(m).encode()))
            .await?;
        Ok(())
    }

    async fn send_frame(&mut self, f: Frame) -> Result<()> {
        self.sink.send(Message::binary(f.encode())).await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Frame> {
        loop {
            let msg = self
                .stream
                .next()
                .await
                .ok_or_else(|| anyhow!("连接已关闭"))??;
            let data = msg.into_data();
            if data.is_empty() {
                continue;
            }
            return Frame::decode(data.as_ref()).map_err(|e| anyhow!("帧解析失败: {e}"));
        }
    }

    async fn recv_timeout(&mut self, secs: u64) -> Result<Frame> {
        match tokio::time::timeout(std::time::Duration::from_secs(secs), self.recv()).await {
            Ok(r) => r,
            Err(_) => bail!("等待对端响应超时（{secs} 秒）"),
        }
    }

    /// 取下一条控制消息，数据帧直接丢弃（推送阶段不该出现）。
    async fn next_control(&mut self, secs: u64) -> Result<Msg> {
        loop {
            match self.recv_timeout(secs).await? {
                Frame::Control(m) => return Ok(m),
                Frame::Chunk(..) => continue,
            }
        }
    }

    /// 处理等待 Ack 期间可能夹杂进来的无关消息，避免它们被丢弃。
    async fn side_effect(&mut self, msg: Msg) -> Result<()> {
        match msg {
            Msg::Mkdir { path } => {
                let _ = std::fs::create_dir_all(join_root(&self.root, &path)?);
            }
            Msg::Delete { path, .. } => {
                remove_path(&self.root, &path)?;
                self.report.deleted += 1;
            }
            _ => {}
        }
        Ok(())
    }

    async fn send_index(&mut self, entries: &[IndexEntry]) -> Result<()> {
        if entries.is_empty() {
            self.send(Msg::IndexChunk {
                seq: 0,
                entries: Vec::new(),
                is_last: true,
            })
            .await?;
            return Ok(());
        }
        let total = entries.len().div_ceil(INDEX_PAGE);
        for (i, chunk) in entries.chunks(INDEX_PAGE).enumerate() {
            self.send(Msg::IndexChunk {
                seq: i as u32,
                entries: chunk.to_vec(),
                is_last: i + 1 == total,
            })
            .await?;
        }
        self.emit(format!("已发送本地索引 {} 条", entries.len()));
        Ok(())
    }

    /// 等待对端上线。超时则报错——GUI 的自动同步不能在这里挂住几分钟。
    ///
    /// 等待期间每 20 秒发一次心跳：服务端把"长时间没有任何数据"判为僵尸连接并关闭，
    /// 而合法等待对端时恰恰是一句话都不说的，不发心跳会被误杀
    /// （客户端只会看到"连接被重置"，无从理解原因）。
    async fn wait_peer(&mut self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + self.peer_timeout;
        loop {
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remain.is_zero() {
                bail!(
                    "等待对端上线超时（{} 秒），对方可能没有运行或配置不一致",
                    self.peer_timeout.as_secs()
                );
            }
            let slice = remain.min(std::time::Duration::from_secs(20));
            let msg = match tokio::time::timeout(slice, self.recv()).await {
                Ok(r) => match r? {
                    Frame::Control(m) => m,
                    Frame::Chunk(..) => continue,
                },
                Err(_) => {
                    // 还不到截止时间 → 这是心跳间隔，不是超时
                    if tokio::time::Instant::now() < deadline {
                        self.send(Msg::KeepAlive).await?;
                        continue;
                    }
                    bail!(
                        "等待对端上线超时（{} 秒），对方可能没有运行或配置不一致",
                        self.peer_timeout.as_secs()
                    );
                }
            };
            match msg {
                Msg::PeerStatus {
                    peer_online: true,
                    peer_device_id,
                } => {
                    self.peer_id = peer_device_id.unwrap_or_default();
                    return Ok(());
                }
                Msg::PeerStatus {
                    peer_online: false, ..
                } => self.emit("对端暂未上线，等待中…"),
                Msg::Bye { reason } => bail!("服务器关闭连接: {reason}"),
                _ => {}
            }
        }
    }

    /// 交换索引。返回 None 表示两端摘要一致，无需同步。
    async fn exchange_index(
        &mut self,
        local: &[IndexEntry],
        local_hash: &str,
        filter_sig: &str,
        strict_filter: bool,
    ) -> Result<Option<Vec<IndexEntry>>> {
        self.emit(format!("对端已上线: {}", short_id(&self.peer_id)));

        let digest = Msg::IndexDigest {
            file_count: local.len() as u64,
            total_bytes: local.iter().map(|e| e.size).sum(),
            root_hash: local_hash.to_string(),
            filter_sig: filter_sig.to_string(),
        };
        self.send(digest).await?;

        let mut entries: Vec<IndexEntry> = Vec::new();
        let mut sent_index = false;

        loop {
            let msg = match self.recv_timeout(300).await? {
                Frame::Control(m) => m,
                Frame::Chunk(..) => continue,
            };

            match msg {
                Msg::IndexDigest {
                    root_hash: rh,
                    filter_sig: peer_sig,
                    ..
                } => {
                    // 过滤规则不一致必须中止，不能"先把不同的传一部分"：
                    // 一方忽略的文件，在另一方眼里就是被删除了，会传播成删除操作。
                    if peer_sig != filter_sig {
                        if peer_sig.is_empty() {
                            if strict_filter {
                                bail!(
                                    "对端版本较旧，不上报过滤规则，而本机已启用过滤。\n\
                                     继续同步可能把被忽略的文件在对端删掉。\n\
                                     请两端都升级到同一版本，或先关闭过滤规则。"
                                );
                            }
                        } else {
                            bail!(
                                "两端的过滤规则不一致，已中止本次同步。\n\
                                 本机：{filter_sig}\n对端：{peer_sig}\n\
                                 过滤规则不一时，被忽略的文件会被对方误判为删除。\n\
                                 请把两台设备的「忽略规则」配成完全一致。"
                            );
                        }
                    }

                    if rh == local_hash {
                        return Ok(None);
                    }
                    if !sent_index {
                        self.send_index(local).await?;
                        sent_index = true;
                    }
                }

                Msg::IndexChunk {
                    entries: chunk,
                    is_last,
                    ..
                } => {
                    entries.extend(chunk);
                    if is_last {
                        self.emit(format!("已接收对端索引 {} 条", entries.len()));
                        return Ok(Some(entries));
                    }
                }

                Msg::Bye { reason } => bail!("服务器关闭连接: {reason}"),
                _ => {}
            }
        }
    }

    /// 推送一批文件。对端不接受就跳过（由对端自己决定）。
    async fn push_entries(&mut self, entries: &[IndexEntry]) -> Result<()> {
        let total = entries.len() as u64;
        for (i, e) in entries.iter().enumerate() {
            self.progress(i as u64, total, e.path.clone());

            if e.is_dir() {
                self.send(Msg::Mkdir {
                    path: e.path.clone(),
                })
                .await?;
                self.note(&e.path, "建目录", true, "");
                continue;
            }

            let full = join_root(&self.root, &e.path)?;
            let cc = chunk_count(e.size);

            self.send(Msg::FileOffer {
                path: e.path.clone(),
                blake3: e.blake3.clone(),
                size: e.size,
                chunk_count: cc,
            })
            .await?;

            // 等对端表态：要么要（Request），要么已有（Ack ok）
            let want = loop {
                match self.next_control(180).await? {
                    Msg::FileRequest { path, .. } if path == e.path => break true,
                    Msg::FileAck { path, .. } if path == e.path => break false,
                    Msg::Bye { reason } => bail!("连接中断: {reason}"),
                    other => self.side_effect(other).await?,
                }
            };
            if !want {
                self.note(&e.path, "上传", true, "对端已存在，跳过");
                continue;
            }

            // 发块过程中文件可能被改，大小对不上就按实际读到的发
            for idx in 0..cc {
                let data = read_chunk(&full, idx)
                    .with_context(|| format!("读取 {} 第 {idx} 块", e.path))?;
                let h = ChunkHeader {
                    path: e.path.clone(),
                    chunk_idx: idx,
                    total: cc,
                    blake3: e.blake3.clone(),
                };
                self.report.bytes_sent += data.len() as u64;
                self.send_frame(Frame::Chunk(h, data)).await?;
            }

            self.send(Msg::FileDone {
                path: e.path.clone(),
                blake3: e.blake3.clone(),
            })
            .await?;

            loop {
                match self.next_control(600).await? {
                    Msg::FileAck { path, ok, .. } if path == e.path => {
                        if ok {
                            self.report.pushed += 1;
                            self.pushed_ok.insert(e.path.clone());
                            self.note(&e.path, "上传", true, "");
                        } else {
                            self.emit(format!("对端未能落地: {}", e.path));
                            self.note(&e.path, "上传", false, "对端未能落地");
                        }
                        break;
                    }
                    Msg::Bye { reason } => bail!("连接中断: {reason}"),
                    other => self.side_effect(other).await?,
                }
            }
        }
        Ok(())
    }

    /// 接收阶段：一直处理到对方发来 PushDone。
    async fn receive_phase(&mut self, conflicts: &HashSet<String>) -> Result<()> {
        let mut incoming: HashMap<String, IncomingFile> = HashMap::new();

        loop {
            let frame = self.recv_timeout(600).await?;

            match frame {
                Frame::Chunk(h, data) => {
                    if let Some(f) = incoming.get_mut(&h.path) {
                        self.report.bytes_recv += data.len() as u64;
                        f.write_chunk(&data)?;
                    }
                }

                Frame::Control(msg) => match msg {
                    Msg::PushDone { .. } => {
                        for (p, f) in incoming.drain() {
                            self.emit(format!("未完成接收，已丢弃: {p}"));
                            self.note(&p, "下载", false, "未完成接收，已丢弃");
                            f.abort();
                        }
                        break;
                    }

                    Msg::Mkdir { path } => {
                        if let Ok(full) = join_root(&self.root, &path) {
                            let _ = std::fs::create_dir_all(full);
                            self.note(&path, "建目录", true, "");
                        }
                    }

                    Msg::FileOffer {
                        path,
                        blake3,
                        chunk_count,
                        ..
                    } => {
                        // 冲突文件：先把本地版本改名保留，再让它被对端版本覆盖
                        if conflicts.contains(&path) {
                            self.stash_local(&path)?;
                        }
                        match IncomingFile::create(&self.root, &path) {
                            Ok(f) => {
                                incoming.insert(path.clone(), f);
                                self.send(Msg::FileRequest {
                                    path,
                                    blake3,
                                    chunk_count,
                                })
                                .await?;
                            }
                            Err(e) => {
                                self.emit(format!("无法创建 {path}: {e}"));
                                self.note(&path, "下载", false, &format!("无法创建: {e}"));
                                self.send(Msg::FileAck {
                                    path,
                                    blake3,
                                    ok: false,
                                })
                                .await?;
                            }
                        }
                    }

                    Msg::FileDone { path, blake3 } => match incoming.remove(&path) {
                        Some(f) => match f.finish(&blake3) {
                            Ok(()) => {
                                self.report.pulled += 1;
                                self.pulled_ok.insert(path.clone());
                                self.note(&path, "下载", true, "");
                                self.send(Msg::FileAck {
                                    path,
                                    blake3,
                                    ok: true,
                                })
                                .await?;
                            }
                            Err(e) => {
                                self.emit(format!("落地失败 {path}: {e}"));
                                self.note(&path, "下载", false, &format!("落地失败: {e}"));
                                self.send(Msg::FileAck {
                                    path,
                                    blake3,
                                    ok: false,
                                })
                                .await?;
                            }
                        },
                        None => {
                            self.send(Msg::FileAck {
                                path,
                                blake3,
                                ok: false,
                            })
                            .await?;
                        }
                    },

                    Msg::Delete { path, .. } => {
                        remove_path(&self.root, &path)?;
                        self.report.deleted += 1;
                        self.note(&path, "删除本地", true, "对端已删除");
                    }

                    Msg::Bye { reason } => bail!("连接中断: {reason}"),
                    _ => {}
                },
            }
        }
        Ok(())
    }

    /// 把本地版本改名为冲突副本，避免被对端版本直接覆盖。
    fn stash_local(&mut self, rel: &str) -> Result<()> {
        let src = join_root(&self.root, rel)?;
        if !src.exists() {
            return Ok(());
        }
        let new_rel = conflict_path(rel, &short_id(&self.device_id), &utc_stamp());
        let dst = join_root(&self.root, &new_rel)?;
        std::fs::rename(&src, &dst)
            .with_context(|| format!("保留冲突副本 {new_rel}"))?;
        self.report.conflicts += 1;
        self.emit(format!("冲突，保留本地副本: {new_rel}"));
        self.note(rel, "冲突副本", true, &new_rel);
        Ok(())
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// 冲突副本用的时间戳（UTC）。不引第三方时间库，自己从 Unix 时间换算。
fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant 的 civil_from_days 算法，把"1970 年以来的天数"转成公历日期。
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + if m <= 2 { 1 } else { 0 }) as i32, m, d)
}

/// 执行一次同步会话。
///
/// `allow_root_change` 为 false 时，如果配置里的目录和上次记录的不一致会直接报错；
/// GUI 在用户确认对话框之后传 true，CLI 由 `--force` 控制。
pub async fn run(
    cfg: &Config,
    tx: Option<Sender<Event>>,
    quiet: bool,
    allow_root_change: bool,
    wait_peer_secs: u64,
) -> Result<Report> {
    let db = cfg.state_db();
    let root_str = cfg.root.replace('\\', "/");
    let filter_sig = cfg.filter.signature();

    // 打开状态库，先做两项"共识是否还有效"的检查，再读 base。
    // 顺序很重要：先清空再加载，否则会拿着已失效的快照算差异。
    let (device_id, base) = {
        let store = Store::open(&db)?;

        // 同步目录变更：base 的键是相对路径，换根目录后旧快照会误判
        if let Some(old) = store.root_path()? {
            if old.replace('\\', "/") != root_str {
                if !allow_root_change {
                    bail!(
                        "同步目录已变更：\n  原目录: {old}\n  新目录: {}\n\
                         更改目录会清空共识快照，下次同步将全量重建基线。\n\
                         确认请加 --force（GUI 会弹窗确认）。",
                        cfg.root
                    );
                }
                store.clear_base()?;
                store.clear_pending()?;
                store.set_root_path(&root_str)?;
                tracing::warn!("同步目录已变更，已清空共识快照");
            }
        } else {
            store.set_root_path(&root_str)?;
        }

        // 过滤规则变更：被忽略的文件会从索引里消失，若继续沿用旧 base，
        // 会被判定为"本地删除了它"并传播成对端的删除。必须重建共识。
        let stored_sig = store.meta("filter_sig")?;
        if stored_sig.as_deref() != Some(filter_sig.as_str()) {
            if stored_sig.is_some() {
                store.clear_base()?;
                store.clear_pending()?;
                tracing::warn!("过滤规则已变更，已清空共识快照，本次将全量重建");
            }
            store.set_meta("filter_sig", &filter_sig)?;
        }

        (store.device_id()?, store.load_base()?)
    };

    let root = PathBuf::from(&cfg.root);
    // 用哈希缓存扫描：大小与 mtime 未变的文件不重复读盘。
    // 之前每 30 秒的轮询会把整个目录读一遍，10 GB 目录就是每 30 秒 10 GB 读。
    let cache = Store::open(&db)?.load_hash_cache().unwrap_or_default();
    let (local, stats, cache) = scan_cached(&root, &cfg.filter, &cache)?;
    if stats.files > 0 {
        // 扫描完立刻回写，哪怕本次会话因为对端不在线而失败，下次也能复用。
        Store::open(&db)?.replace_hash_cache(&cache)?;
    }
    let scan_msg = format!(
        "扫描 {} 个文件：复用 {} 个哈希，重算 {} 个，{} ms",
        stats.files, stats.reused, stats.hashed, stats.duration_ms
    );
    let local_hash = root_hash(&local);
    let local_map: HashMap<String, IndexEntry> = local
        .iter()
        .map(|e| (e.path.clone(), e.clone()))
        .collect();

    let (ws, _) = tokio_tungstenite::connect_async(cfg.url().as_str())
        .await
        .context("连接服务器失败")?;
    let (sink, stream) = ws.split();

    let mut sess = Session {
        sink,
        stream,
        root: root.clone(),
        device_id: device_id.clone(),
        peer_id: String::new(),
        tx,
        report: Report::default(),
        quiet,
        peer_timeout: std::time::Duration::from_secs(wait_peer_secs.max(1)),
        pushed_ok: HashSet::new(),
        pulled_ok: HashSet::new(),
    };

    sess.send(Msg::Hello {
        device_id: device_id.clone(),
        workspace_id: cfg.workspace.clone(),
        proto_ver: PROTO_VER,
    })
    .await?;
    sess.emit(scan_msg);

    // 对端索引的 (路径 -> 内容哈希)，用于事后判定"哪些路径双方确实一致"
    let mut peer_hash: HashMap<String, String> = HashMap::new();

    let report = match {
        sess.wait_peer().await?;
        sess.exchange_index(&local, &local_hash, &filter_sig, !cfg.filter.is_empty())
            .await?
    } {
        None => {
            sess.emit("两端内容一致，无需同步");
            // 摘要相同即两端完全一致，全部路径都可以确认
            peer_hash = local_map
                .iter()
                .map(|(k, v)| (k.clone(), v.blake3.clone()))
                .collect();
            let mut r = std::mem::take(&mut sess.report);
            r.up_to_date = true;
            r
        }
        Some(peer_entries) => {
            // 两台设备共用同一个 device_id 时（通常是拷了 state.db），
            // 双方都会判定自己是 responder，各等对方发数据，静默死锁到超时。
            // 这里提前拦下来，给出能照着做的提示。
            if sess.peer_id == device_id {
                bail!(
                    "两台设备使用了相同的 device_id（{device_id}）。\n\
                     通常是复制了 %APPDATA%\\WinSync\\state.db 导致的。\n\
                     请在任意一端删除该文件后重启程序，它会生成新的设备标识。"
                );
            }

            let remote_map: HashMap<String, IndexEntry> = peer_entries
                .into_iter()
                .map(|e| (e.path.clone(), e))
                .collect();
            peer_hash = remote_map
                .iter()
                .map(|(k, v)| (k.clone(), v.blake3.clone()))
                .collect();
            let mut plan = diff(&base, &local_map, &remote_map);

            // 按同步模式裁剪计划：单向模式下主动放弃另一个方向的动作。
            // 这里只是"不做"，不改 base——被放弃的差异下次还会被算出，
            // 属于预期行为（单向模式本来就永远不收敛）。
            match cfg.mode {
                SyncMode::Bidirectional => {}
                SyncMode::UploadOnly => {
                    let dropped = plan.pull.len() + plan.delete_local.len() + plan.conflicts.len();
                    plan.pull.clear();
                    plan.delete_local.clear();
                    plan.conflicts.clear();
                    if dropped > 0 {
                        sess.emit(format!(
                            "仅上传模式：忽略 {dropped} 项对端变更（不拉取、不删除本地）"
                        ));
                    }
                }
                SyncMode::DownloadOnly => {
                    let dropped = plan.push.len() + plan.delete_remote.len() + plan.conflicts.len();
                    plan.push.clear();
                    plan.delete_remote.clear();
                    plan.conflicts.clear();
                    if dropped > 0 {
                        sess.emit(format!(
                            "仅下载模式：忽略 {dropped} 项本机变更（不上传、不删除对端）"
                        ));
                    }
                }
            }

            sess.emit(format!(
                "差异：推送 {}，拉取 {}，删除 {}，冲突 {}",
                plan.push.len(),
                plan.pull.len(),
                plan.delete_local.len() + plan.delete_remote.len(),
                plan.conflicts.len()
            ));

            let conflicts: HashSet<String> =
                plan.conflicts.iter().map(|e| e.path.clone()).collect();

            // ---- 删除安全阀 ----
            // 根目录配置不一致时，"对面没有这个文件"会被误判成"对面删了它"，
            // 从而把本机文件成批删掉。这类事故一旦发生就无法回滚（直接删除不进回收站），
            // 所以超过阈值就中止，把要删的路径列出来让人确认。
            let dels = plan.delete_local.len() + plan.delete_remote.len();
            if dels > 0 {
                let base_len = base.len();
                let pct = if base_len == 0 {
                    100
                } else {
                    dels * 100 / base_len
                };
                let too_many = dels > 20 || (base_len >= 5 && pct > 30);

                if too_many {
                    let mut detail = String::new();
                    for p in plan.delete_local.iter().take(10) {
                        detail.push_str(&format!("\n  - {p}"));
                    }
                    for p in plan.delete_remote.iter().take(10) {
                        detail.push_str(&format!("\n  - {p}"));
                    }
                    bail!(
                        "本次同步将删除 {dels} 个文件（当前基线 {base_len} 项，占 {pct}%），已中止。\
                         \n\n常见原因：两台机器的同步目录配置不一致，导致互相判定对方删除了文件。{detail}\
                         \n\n请先确认两端的同步目录是不是同一个文件夹。"
                    );
                }

                sess.emit(format!(
                    "本次将删除 {dels} 个文件（基线 {base_len} 项）"
                ));
            }

            let is_initiator = device_id < sess.peer_id;

            if is_initiator {
                sess.push_entries(&plan.push).await?;
                sess.send(Msg::PushDone {
                    device_id: device_id.clone(),
                })
                .await?;
                sess.receive_phase(&conflicts).await?;
            } else {
                sess.receive_phase(&conflicts).await?;
                sess.push_entries(&plan.push).await?;
                sess.send(Msg::PushDone {
                    device_id: device_id.clone(),
                })
                .await?;
            }

            for p in &plan.delete_remote {
                sess.send(Msg::Delete {
                    path: p.clone(),
                    is_dir: false,
                })
                .await?;
                sess.note(p, "删除对端", true, "本机已删除");
            }
            for p in &plan.delete_local {
                remove_path(&sess.root, p)?;
                sess.report.deleted += 1;
                sess.note(p, "删除本地", true, "对端已删除");
            }

            std::mem::take(&mut sess.report)
        }
    };

    let pushed_ok = std::mem::take(&mut sess.pushed_ok);
    let pulled_ok = std::mem::take(&mut sess.pulled_ok);
    drop(sess);

    // 会话成功，写入新的共识基线；同时落盘本次的文件级记录。
    //
    // 基线必须是「双方确认一致的路径」，不能简单等于本地扫描结果：
    // 单向模式下被刻意跳过的文件只存在于一方，若写进基线，等切回双向时
    // 另一方会判定"对端把它删了"，于是删除本地文件。这个坑必须从语义上堵住。
    {
        let mut store = Store::open(&db)?;
        let (final_entries, _, fresh_cache) = scan_cached(&root, &cfg.filter, &cache)?;
        store.replace_hash_cache(&fresh_cache)?;
        let confirmed: Vec<IndexEntry> = final_entries
            .into_iter()
            .filter(|e| {
                // 本次推给对方且对方确认落地
                pushed_ok.contains(&e.path)
                    // 本次从对方拉取并落地成功
                    || pulled_ok.contains(&e.path)
                    // 或者会话开始时两端内容本来就相同
                    || peer_hash
                        .get(&e.path)
                        .is_some_and(|h| *h == e.blake3)
            })
            .collect();
        store.replace_base(&confirmed)?;
        store.push_activities(&report.activities)?;
        store.prune_activities(2000)?;
    }

    Ok(report)
}
