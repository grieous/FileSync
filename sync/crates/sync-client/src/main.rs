//! 命令行入口。
//!
//! 图形界面是主入口（winsync-gui），这个 CLI 用于排查问题和跑自动化：
//!   sync scan   只看本地索引与状态，不联网
//!   sync sync   执行一次真实同步
//!   sync where  打印配置与状态库位置
//!
//! 配置与 GUI 共用 `%APPDATA%\WinSync\config.json`，命令行参数可以临时覆盖。

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use sync_client::{run, Config};
use sync_core::{root_hash, scan, Store};

#[derive(Parser, Debug)]
#[command(name = "sync", version, about = "两台 Windows 经 Linux 中继双向同步")]
struct Args {
    #[arg(long, env = "SYNC_SERVER")]
    server: Option<String>,

    #[arg(long, env = "SYNC_TOKEN")]
    token: Option<String>,

    #[arg(long, env = "SYNC_WORKSPACE")]
    workspace: Option<String>,

    #[arg(long, env = "SYNC_ROOT")]
    root: Option<PathBuf>,

    /// 更改同步目录时确认清空共识快照
    #[arg(long)]
    force: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 只扫描本地目录，不上网
    Scan,
    /// 执行一次同步
    Sync,
    /// 打印配置文件与状态库位置
    Where,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_console();
    let args = Args::parse();

    match args.cmd {
        Cmd::Where => {
            println!("配置文件 : {}", Config::file().display());
            println!("状态库   : {}", Config::state_db().display());
            Ok(())
        }
        Cmd::Scan => cmd_scan(&args),
        Cmd::Sync => cmd_sync(&args).await,
    }
}

fn resolve(args: &Args) -> Result<Config> {
    let mut cfg = Config::load()?.unwrap_or_default();
    if let Some(v) = &args.server {
        cfg.server = v.clone();
    }
    if let Some(v) = &args.token {
        cfg.token = v.clone();
    }
    if let Some(v) = &args.workspace {
        cfg.workspace = v.clone();
    }
    if let Some(v) = &args.root {
        cfg.root = v.to_string_lossy().to_string();
    }

    if cfg.server.is_empty() || cfg.token.is_empty() || cfg.root.is_empty() {
        bail!(
            "配置不完整。请先启动图形界面填写，或提供参数：\n\
             --server ws://host:port/ws --token <密钥> --root \"D:\\Sync\""
        );
    }
    if !Path::new(&cfg.root).is_dir() {
        bail!("同步目录不存在: {}", cfg.root);
    }

    cfg.save()?;
    Ok(cfg)
}

fn cmd_scan(args: &Args) -> Result<()> {
    let cfg = resolve(args)?;
    let entries = scan(Path::new(&cfg.root))?;
    let files = entries.iter().filter(|e| !e.is_dir()).count();
    let bytes: u64 = entries.iter().map(|e| e.size).sum();

    let store = Store::open(&Config::state_db())?;

    println!("同步目录 : {}", cfg.root);
    println!("服务器   : {}", cfg.server);
    println!("房间     : {}", cfg.workspace);
    println!("device_id: {}", store.device_id()?);
    println!("base 条目: {}", store.base_count()?);
    println!("目录     : {}", entries.len() - files);
    println!("文件     : {files}");
    println!("总字节   : {bytes}");
    println!("root_hash: {}", root_hash(&entries));
    Ok(())
}

async fn cmd_sync(args: &Args) -> Result<()> {
    let cfg = resolve(args)?;
    println!("同步 {} ⇄ {}", cfg.root, cfg.server);

    let report = run(&cfg, None, false, args.force, 300).await?;

    if report.up_to_date {
        println!("结果：两端已一致，无需传输");
    } else {
        println!("---- 结果 ----");
        println!("推送   {:>4} 个", report.pushed);
        println!("拉取   {:>4} 个", report.pulled);
        println!("删除   {:>4} 个", report.deleted);
        println!("冲突   {:>4} 个", report.conflicts);
        println!("上行   {:>4} KB", report.bytes_sent / 1024);
        println!("下行   {:>4} KB", report.bytes_recv / 1024);
    }
    Ok(())
}

fn init_console() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{SetConsoleCP, SetConsoleOutputCP};
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
    }
}
