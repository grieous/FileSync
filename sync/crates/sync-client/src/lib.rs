//! 客户端库：配置 + 一次完整同步会话。

pub mod config;
pub mod session;

pub use config::Config;
pub use session::{run, Event, Report};
