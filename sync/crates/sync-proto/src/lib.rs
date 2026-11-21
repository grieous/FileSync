//! 协议定义：控制消息（JSON）+ 文件分块（二进制帧）。
//!
//! 每一帧对应一条 WebSocket 二进制消息：
//! ```text
//! [0]      tag: 0=Control, 1=Chunk
//! Control: tag 之后是 UTF-8 JSON
//! Chunk  : tag, u32be header_len, header JSON, payload bytes
//! ```
//!
//! 服务端不解析 Chunk 的 payload，只按 workspace 原样转发。

use serde::{Deserialize, Serialize};

pub const PROTO_VER: u32 = 1;

/// 文件分块大小。4 GB 文件 = 1024 块。
pub const CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// 单条 IndexChunk 最多携带的条目数，避免单帧过大。
pub const INDEX_PAGE: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    File,
    Dir,
}

impl Kind {
    pub fn as_u8(self) -> u8 {
        match self {
            Kind::File => 0,
            Kind::Dir => 1,
        }
    }

    pub fn from_u8(v: i64) -> Self {
        if v == 1 {
            Kind::Dir
        } else {
            Kind::File
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// POSIX 风格相对路径，`/` 分隔，无前导斜杠。
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    /// 文件内容 BLAKE3（小写 hex）。目录为空串。
    pub blake3: String,
    pub mtime: i64,
}

impl IndexEntry {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir)
    }
}

/// 控制消息。两端对等使用，服务端只读 `Hello`，其余原样转发。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    Hello {
        device_id: String,
        workspace_id: String,
        proto_ver: u32,
    },
    PeerStatus {
        peer_online: bool,
        peer_device_id: Option<String>,
    },
    Bye {
        reason: String,
    },

    /// 等待对端期间的心跳。服务端靠"收到任何数据"判断连接活着，
    /// 不发送它的话，单独等待的一方会在空闲 180 秒后被当成僵尸踢掉。
    KeepAlive,

    IndexDigest {
        file_count: u64,
        total_bytes: u64,
        root_hash: String,
        /// 过滤规则签名。两端必须一致——一方忽略的文件在另一方看来是"被删除"，
        /// 会把删除传播回去。旧版本没有这个字段，缺省为空表示"未声明"。
        #[serde(default)]
        filter_sig: String,
    },
    IndexChunk {
        seq: u32,
        entries: Vec<IndexEntry>,
        is_last: bool,
    },
    IndexAck {
        root_hash: String,
    },

    FileOffer {
        path: String,
        blake3: String,
        size: u64,
        chunk_count: u32,
    },
    FileRequest {
        path: String,
        blake3: String,
        chunk_count: u32,
    },
    FileDone {
        path: String,
        blake3: String,
    },
    FileAck {
        path: String,
        blake3: String,
        ok: bool,
    },

    /// 一方把本次要推送的文件全部发完后，通知对端"轮到你了"。
    /// 单向串行传输，避免双向并发把状态机搞乱。
    PushDone {
        device_id: String,
    },

    Mkdir {
        path: String,
    },
    Delete {
        path: String,
        is_dir: bool,
    },
    Rename {
        old_path: String,
        new_path: String,
    },
    Conflict {
        path: String,
        reason: String,
        local_hash: String,
        remote_hash: String,
    },

    SessionEnd {
        session_id: String,
        base_hash: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkHeader {
    pub path: String,
    pub chunk_idx: u32,
    pub total: u32,
    pub blake3: String,
}

#[derive(Debug, Clone)]
pub enum Frame {
    Control(Msg),
    Chunk(ChunkHeader, Vec<u8>),
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("frame too short")]
    Short,
    #[error("unknown frame tag {0}")]
    UnknownTag(u8),
    #[error("bad chunk header length")]
    BadHeaderLen,
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

const TAG_CONTROL: u8 = 0;
const TAG_CHUNK: u8 = 1;

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Frame::Control(m) => {
                let mut out = Vec::with_capacity(256);
                out.push(TAG_CONTROL);
                serde_json::to_writer(&mut out, m).expect("control message is serializable");
                out
            }
            Frame::Chunk(h, payload) => {
                let header = serde_json::to_vec(h).expect("chunk header is serializable");
                let mut out = Vec::with_capacity(5 + header.len() + payload.len());
                out.push(TAG_CHUNK);
                out.extend_from_slice(&(header.len() as u32).to_be_bytes());
                out.extend_from_slice(&header);
                out.extend_from_slice(payload);
                out
            }
        }
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        let (&tag, rest) = buf.split_first().ok_or(ProtoError::Short)?;
        match tag {
            TAG_CONTROL => Ok(Frame::Control(serde_json::from_slice(rest)?)),
            TAG_CHUNK => {
                if rest.len() < 4 {
                    return Err(ProtoError::Short);
                }
                let hl = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
                if rest.len() < 4 + hl {
                    return Err(ProtoError::BadHeaderLen);
                }
                let header: ChunkHeader = serde_json::from_slice(&rest[4..4 + hl])?;
                Ok(Frame::Chunk(header, rest[4 + hl..].to_vec()))
            }
            other => Err(ProtoError::UnknownTag(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_roundtrip() {
        let m = Msg::PeerStatus {
            peer_online: true,
            peer_device_id: Some("abc".into()),
        };
        let bytes = Frame::Control(m).encode();
        match Frame::decode(&bytes).unwrap() {
            Frame::Control(Msg::PeerStatus { peer_online, .. }) => assert!(peer_online),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn chunk_roundtrip() {
        let h = ChunkHeader {
            path: "a/b.txt".into(),
            chunk_idx: 3,
            total: 10,
            blake3: "deadbeef".into(),
        };
        let payload = vec![1u8, 2, 3, 4, 5];
        let bytes = Frame::Chunk(h, payload.clone()).encode();
        match Frame::decode(&bytes).unwrap() {
            Frame::Chunk(hh, pp) => {
                assert_eq!(hh.chunk_idx, 3);
                assert_eq!(pp, payload);
            }
            _ => panic!("wrong variant"),
        }
    }
}
