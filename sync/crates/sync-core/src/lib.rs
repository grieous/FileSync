//! 两端共用的同步内核：本地状态库、文件扫描、三方差异计算、文件落地。

pub mod db;
pub mod fileio;
pub mod index;

pub use db::Store;
pub use fileio::{chunk_count, conflict_path, join_root, read_chunk, remove_path, IncomingFile};
pub use index::{diff, hash_file, root_hash, scan, to_rel, Plan};
