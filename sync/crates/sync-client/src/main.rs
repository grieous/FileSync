//! 命令行入口。
//!
//! 图形界面是主入口（winsync-gui），这个 CLI 用于排查问题和跑自动化：
//!   sync tasks   列出已配置的同步任务
//!   sync scan    只看本地索引与状态，不联网
//!   sync sync    执行一次真实同步
//!   sync where   打印配置与状态库位置
//!
//! 配置与 GUI 共用 `%APPDATA%\WinSync\config.json`；
//! `--task` 选择任务，`--server/--token/--root` 可临时覆盖（不写回配置）。

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use sync_client::{run, Config, Limits, Settings};
use sync_core::{root_hash, scan_cached, Store};

#[derive(Parser, Debug)]
#[command(name = "sync", version, about = "两台 Windows 经 Linux 中继双向同步")]
struct Args {
    /// 任务 id 或任务名（默认第一个任务）
    #[arg(long, env = "SYNC_TASK")]
    task: Option<String>,

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

    /// 等待对端上线的秒数（默认 300）
    #[arg(long, default_value_t = 300)]
    wait: u64,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 列出已配置的任务
    Tasks,
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
            let settings = Settings::load()?.unwrap_or_default();
            println!("配置文件 : {}", Config::file().display());
            println!("状态目录 : {}", Config::dir().join("tasks").display());
            if settings.tasks.is_empty() {
                println!("任务     : （无，请先在界面里新建）");
            }
            for t in &settings.tasks {
                println!(
                    "任务 {:<10} {:<12} {} ⇄ {}",
                    t.id,
                    t.display_name(),
                    t.root,
                    t.workspace
                );
                println!("           状态库: {}", t.state_db().display());
            }
            Ok(())
        }
        Cmd::Tasks => {
            let settings = Settings::load()?.unwrap_or_default();
            if settings.tasks.is_empty() {
                println!("还没有任务。请在图形界面里点「新建任务」。");
                return Ok(());
            }
            println!(
                "{:<10} {:<14} {:<8} {:<8} {}",
                "ID", "名称", "模式", "状态", "目录"
            );
            for t in &settings.tasks {
                println!(
                    "{:<10} {:<14} {:<8} {:<8} {}",
                    t.id,
                    t.display_name(),
                    match t.mode {
                        sync_client::SyncMode::Bidirectional => "双向",
                        sync_client::SyncMode::UploadOnly => "仅上传",
                        sync_client::SyncMode::DownloadOnly => "仅下载",
                    },
                    if t.paused { "已暂停" } else { "运行中" },
                    t.root
                );
            }
            Ok(())
        }
        Cmd::Scan => cmd_scan(&args),
        Cmd::Sync => cmd_sync(&args).await,
    }
}

/// 选出一个任务并套用命令行覆盖。不写回配置文件——界面里的任务不该被 CLI 改掉。
fn resolve(args: &Args) -> Result<Config> {
    let settings = Settings::load()?.unwrap_or_default();

    let mut task = if let Some(sel) = &args.task {
        match settings.find(sel) {
            Some(i) => settings.tasks[i].clone(),
            None => bail!("找不到任务 {sel}。用 `sync tasks` 查看已配置的任务。"),
        }
    } else if let Some(t) = settings.tasks.first() {
        t.clone()
    } else {
        Config::default()
    };

    // 命令行覆盖（自动化测试、临时排查都靠它）
    if let Some(v) = &args.server {
        task.server = v.clone();
    }
    if let Some(v) = &args.token {
        task.token = v.clone();
    }
    if let Some(v) = &args.workspace {
        task.workspace = v.clone();
    }
    if let Some(v) = &args.root {
        task.root = v.to_string_lossy().to_string();
    }
    if args.server.is_some() || args.root.is_some() {
        task.id = "adhoc".to_string();
        task.name = "临时任务".to_string();
    }

    let issues = task.issues();
    if !issues.is_empty() {
        bail!(
            "任务配置不完整：{}\n可在图形界面里新建/编辑任务，或用参数覆盖：\n\
             --server ws://host:port/ws --token <密钥> --root \"D:\\Sync\"",
            issues.join("；")
        );
    }
    Ok(task)
}

fn cmd_scan(args: &Args) -> Result<()> {
    let cfg = resolve(args)?;
    let store = Store::open(&cfg.state_db())?;
    let cache = store.load_hash_cache()?;
    let (entries, stats, fresh) = scan_cached(Path::new(&cfg.root), &cfg.filter, &cache)?;
    store.replace_hash_cache(&fresh)?;

    let files = entries.iter().filter(|e| !e.is_dir()).count();
    let bytes: u64 = entries.iter().map(|e| e.size).sum();

    println!("任务     : {} ({})", cfg.display_name(), cfg.id);
    println!("同步目录 : {}", cfg.root);
    println!("服务器   : {}", cfg.server);
    println!("房间     : {}", cfg.workspace);
    println!("模式     : {:?}", cfg.mode);
    println!("状态     : {}", if cfg.paused { "已暂停" } else { "运行中" });
    println!("过滤规则 : {}", cfg.filter.signature());
    println!("状态库   : {}", cfg.state_db().display());
    println!("device_id: {}", store.device_id()?);
    println!("base 条目: {}", store.base_count()?);
    println!("目录     : {}", entries.len() - files);
    println!("文件     : {files}");
    println!("总字节   : {bytes}");
    println!(
        "扫描耗时 : {} ms（复用 {} 个哈希，重算 {} 个）",
        stats.duration_ms, stats.reused, stats.hashed
    );
    println!("root_hash: {}", root_hash(&entries));
    Ok(())
}

async fn cmd_sync(args: &Args) -> Result<()> {
    let cfg = resolve(args)?;
    println!("同步 {} ⇄ {}", cfg.root, cfg.server);

    let report = run(&cfg, None, false, args.force, args.wait, Limits::default()).await?;

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
