//! WinSync 图形界面。
//!
//! 版式对齐飞牛同步客户端（深色、左侧栏 + 右侧内容）：
//!   同步任务 —— 任务卡列表（路径 / 目标 / 规则 三列）
//!   同步日志 —— 文件级记录表（文件名称 / 任务 / 时间 / 状态 / 事件）
//!   设置     —— 开机自启、更新、日志清理、限速
//!
//! 托盘在自己的线程里泵 Windows 消息，不依赖界面刷新。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use futures_util::{SinkExt, StreamExt};
use notify::Watcher;
use sync_client::config::{new_id, Settings, SyncMode};
use sync_client::session::{self, Event, Limits, Report};
use sync_client::Config;
use sync_core::{Activity, Filter, Store};
use sync_proto::{Frame as WireFrame, Msg, PROTO_VER};

enum WorkerMsg {
    Event { task: String, ev: Event },
    Done { task: String, res: Result<Report, String> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Tasks,
    Log,
    Settings,
}

/// 取色自飞牛同步客户端：近黑底、深灰卡片、亮蓝主色。
mod ui_colors {
    use eframe::egui::Color32;

    pub const APP: Color32 = Color32::from_rgb(0x0E, 0x0F, 0x10);
    pub const SIDEBAR: Color32 = Color32::from_rgb(0x16, 0x17, 0x19);
    pub const CARD: Color32 = Color32::from_rgb(0x1C, 0x1D, 0x1F);
    pub const CARD_HOVER: Color32 = Color32::from_rgb(0x23, 0x24, 0x27);
    pub const BORDER: Color32 = Color32::from_rgb(0x2A, 0x2C, 0x2E);
    pub const TEXT: Color32 = Color32::from_rgb(0xED, 0xED, 0xEE);
    pub const TEXT_2: Color32 = Color32::from_rgb(0xA0, 0xA2, 0xA6);
    pub const TEXT_3: Color32 = Color32::from_rgb(0x6E, 0x71, 0x76);
    pub const PRIMARY: Color32 = Color32::from_rgb(0x2B, 0x6C, 0xF6);
    pub const PRIMARY_DIM: Color32 = Color32::from_rgb(0x1E, 0x35, 0x5C);
    pub const SUCCESS: Color32 = Color32::from_rgb(0x4C, 0xC3, 0x8A);
    pub const DANGER: Color32 = Color32::from_rgb(0xF0, 0x55, 0x4D);
    pub const WARN: Color32 = Color32::from_rgb(0xE6, 0xA2, 0x3C);
}

/// 仅 debug 构建：设 WINSYNC_GUI_SELFTEST=1 时启动就打开向导、画 N 帧后退出。
/// 用来在没有人点鼠标的情况下真正走一遍第一步的布局（能抓到 panic）。
#[cfg(debug_assertions)]
static SELFTEST_FRAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/* ------------------------------------------------------------ 配对（旁路房间） */

/// 配对阶段，语义与 dsh 插件一致：**一次性握手**，配上过就不再回退门禁。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PairPhase {
    Idle,
    Connecting,
    Waiting,
    Paired,
    Denied,
    Unreachable,
}

impl PairPhase {
    fn title(self, ever_paired: bool) -> &'static str {
        match self {
            PairPhase::Paired => "配对已通过",
            PairPhase::Waiting if ever_paired => "已配对过，对端当前不在线",
            PairPhase::Waiting => "已连上中继，正在等对方",
            PairPhase::Connecting => "正在连接中继…",
            PairPhase::Denied => "中继拒绝了配对通道",
            PairPhase::Unreachable => "连不上中继",
            PairPhase::Idle => "还没开始配对",
        }
    }
}

/// 配对线程发给界面的消息；带 gen，过期的直接丢。
#[derive(Clone)]
enum PairEvent {
    /// (阶段, 对端标识, 错误原文)
    Phase(PairPhase, String, String),
    /// 创建端挂在 device id 上带过来的「房间码, 密钥」
    Carried(String, String),
}

fn hex_of(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

/// 对端 device id 里 `~` 后面那段十六进制是创建端塞进来的「房间码|密钥」，
/// 解出来加入端直接填上，不用手抄 32 位。
fn parse_announce(device: &str) -> Option<(String, String)> {
    let (_, hex) = device.split_once('~')?;
    if hex.len() % 2 != 0 || hex.is_empty() {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&hex[i..i + 2], 16).ok()?);
    }
    let text = String::from_utf8(bytes).ok()?;
    let (room, token) = text.split_once('|')?;
    Some((room.to_string(), token.to_string()))
}

/// 起一条配对守护线程：连 `<通道码>::pair`、发 Hello、读 PeerStatus。
///
/// 为什么不直接复用 daemon 那份：daemon 是全局单例状态 + HTTP 轮询，GUI 是每帧轮询界面状态，
/// 接法不同；协议这一段（Hello / PeerStatus / Bye）照搬，语义不另起一套。
fn spawn_pairing(
    gen: u64,
    url: String,
    room: String,
    device: String,
    tx: Sender<(u64, PairEvent)>,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };
        rt.block_on(async move {
            let side_room = format!("{room}::pair");
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let _ = tx.send((
                    gen,
                    PairEvent::Phase(PairPhase::Connecting, String::new(), String::new()),
                ));
                match tokio_tungstenite::connect_async(url.as_str()).await {
                    Err(err) => {
                        let denied = matches!(
                            &err,
                            tokio_tungstenite::tungstenite::Error::Http(resp) if resp.status() == 401
                        );
                        let msg = if denied {
                            "中继拒绝了配对通道：中继若开了房间级密钥，配对房间也要放行（给 SYNC_ROOMS 加一条 \"*\" 兜底密钥）".to_string()
                        } else {
                            format!("连不上 {url}：{err}")
                        };
                        let phase = if denied {
                            PairPhase::Denied
                        } else {
                            PairPhase::Unreachable
                        };
                        let _ = tx.send((gen, PairEvent::Phase(phase, String::new(), msg)));
                        tokio::time::sleep(Duration::from_millis(if denied { 5000 } else { 2000 }))
                            .await;
                    }
                    Ok((mut socket, _)) => {
                        let hello = WireFrame::Control(Msg::Hello {
                            device_id: device.clone(),
                            workspace_id: side_room.clone(),
                            proto_ver: PROTO_VER,
                        });
                        if socket
                            .send(tokio_tungstenite::tungstenite::Message::Binary(
                                hello.encode(),
                            ))
                            .await
                            .is_err()
                        {
                            tokio::time::sleep(Duration::from_millis(1200)).await;
                            continue;
                        }
                        let _ = tx.send((
                            gen,
                            PairEvent::Phase(PairPhase::Waiting, String::new(), String::new()),
                        ));
                        loop {
                            if stop.load(Ordering::Relaxed) {
                                let _ = socket.close(None).await;
                                return;
                            }
                            // 500ms 醒一次：点「重新配对」或关向导时能立刻退
                            let next = tokio::time::timeout(
                                Duration::from_millis(500),
                                socket.next(),
                            )
                            .await;
                            let msg = match next {
                                Ok(Some(Ok(msg))) => msg,
                                Ok(Some(Err(_))) | Ok(None) => break,
                                Err(_) => continue,
                            };
                            match msg {
                                tokio_tungstenite::tungstenite::Message::Binary(data) => {
                                    match WireFrame::decode(&data) {
                                        Ok(WireFrame::Control(Msg::PeerStatus {
                                            peer_online,
                                            peer_device_id,
                                        })) => {
                                            let peer = peer_device_id.unwrap_or_default();
                                            if peer_online {
                                                if let Some((room, token)) = parse_announce(&peer) {
                                                    let _ =
                                                        tx.send((gen, PairEvent::Carried(room, token)));
                                                }
                                            }
                                            let phase = if peer_online {
                                                PairPhase::Paired
                                            } else {
                                                PairPhase::Waiting
                                            };
                                            let _ = tx.send((
                                                gen,
                                                PairEvent::Phase(phase, peer, String::new()),
                                            ));
                                        }
                                        // 中继赶客（房间满 / 顶号）：原话转给界面，别当普通掉线
                                        Ok(WireFrame::Control(Msg::Bye { reason })) => {
                                            let _ = tx.send((
                                                gen,
                                                PairEvent::Phase(PairPhase::Denied, String::new(), reason),
                                            ));
                                            return;
                                        }
                                        _ => {}
                                    }
                                }
                                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                                _ => {}
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(1200)).await;
                    }
                }
            }
        });
    });
}

struct Runtime {
    running: bool,
    next_auto: Instant,
    /// (已传字节, 总字节, 文件名, 上行 B/s, 下行 B/s)
    progress: Option<(u64, u64, String, u64, u64)>,
    last: Option<Result<Report, String>>,
    last_time: Option<String>,
    logs: Vec<String>,
    watcher: Option<notify::RecommendedWatcher>,
    watch_root: String,
    dirty: bool,
    last_change: Instant,
}

impl Runtime {
    fn new() -> Self {
        Self {
            running: false,
            next_auto: Instant::now() + Duration::from_secs(5),
            progress: None,
            last: None,
            last_time: None,
            logs: Vec::new(),
            watcher: None,
            watch_root: String::new(),
            dirty: false,
            last_change: Instant::now(),
        }
    }
}

struct EditBuf {
    name: String,
    /// 中继地址分两栏：IP 与端口分开填，拼成 URL 的活交给 compose_server
    host: String,
    port: String,
    /// 房间码默认打码：它是门禁，不该随手出现在截屏里
    reveal_room: bool,
    token: String,
    workspace: String,
    root: String,
    mode: SyncMode,
    dotfiles: bool,
    ext: String,
    min_mb: f64,
    max_mb: f64,
}

impl EditBuf {
    fn from_task(t: &Config) -> Self {
        Self {
            name: t.name.clone(),
            host: split_server(&t.server).0,
            port: split_server(&t.server).1,
            reveal_room: false,
            token: t.token.clone(),
            workspace: t.workspace.clone(),
            root: t.root.clone(),
            mode: t.mode,
            dotfiles: t.filter.ignore_dotfiles,
            ext: t.filter.ignore_extensions.join(","),
            min_mb: t.filter.size_min.map(|b| b as f64 / MB).unwrap_or(0.0),
            max_mb: t.filter.size_max.map(|b| b as f64 / MB).unwrap_or(0.0),
        }
    }

    fn filter(&self) -> Filter {
        Filter {
            ignore_dotfiles: self.dotfiles,
            ignore_extensions: self
                .ext
                .split(',')
                .map(|s| s.trim().trim_start_matches('.').to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            size_min: if self.min_mb > 0.0 {
                Some((self.min_mb * MB) as u64)
            } else {
                None
            },
            size_max: if self.max_mb > 0.0 {
                Some((self.max_mb * MB) as u64)
            } else {
                None
            },
        }
    }

    fn apply_to(&self, t: &mut Config) {
        t.name = self.name.trim().to_string();
        t.server = compose_server(&self.host, &self.port);
        t.workspace = self.workspace.trim().to_string();
        // 密钥留空 = 跟随房间码，与新建流程保持同一语义
        t.token = if self.token.trim().is_empty() {
            t.workspace.clone()
        } else {
            self.token.trim().to_string()
        };
        t.root = self.root.trim().to_string();
        t.mode = self.mode;
        t.filter = self.filter();
    }
}

/// 新建任务向导：1 同步设备 → 2 规则设置 → 3 同步路径（与飞牛同步一致）
struct Wizard {
    step: u8,
    /// 0 = 创建新同步，1 = 加入已有同步（与插件的两个入口一致）
    entry: u8,
    /// 6 位通道码：配对只交换这串数字，房间码由配对通道自己带过去
    channel: String,
    host: String,
    port: String,
    reveal_room: bool,
    token: String,
    workspace: String,
    mode: SyncMode,
    dotfiles: bool,
    ext: String,
    root: String,
    name: String,
    note: String,
    // ---- 配对 ----
    pair_phase: PairPhase,
    pair_peer: String,
    pair_error: String,
    /// 配上过就是配上过：对端随后离开不把门禁关回去
    ever_paired: bool,
    pair_gen: u64,
    /// 当前这条守护线程的「配置指纹」，变了就重开
    pair_key: String,
    pair_stop: Option<Arc<AtomicBool>>,
    /// 配置刚改过就等 500ms 再起线程：否则每敲一个字符都会起一条
    pair_pending: Option<Instant>,
}

impl Wizard {
    fn new(seed: Option<&Config>) -> Self {
        Self {
            step: 0,
            entry: 0,
            channel: new_channel(),
            host: seed.map(|t| split_server(&t.server).0).unwrap_or_default(),
            port: seed.map(|t| split_server(&t.server).1).unwrap_or_default(),
            reveal_room: false,
            token: seed.map(|t| t.token.clone()).unwrap_or_default(),
            // 没沿用旧任务就自动生成房间码：不用起名字，也不用去中继登记（与插件一致）
            workspace: seed
                .map(|t| t.workspace.clone())
                .unwrap_or_else(new_room_code),
            mode: SyncMode::Bidirectional,
            dotfiles: false,
            ext: String::new(),
            root: String::new(),
            name: String::new(),
            note: String::new(),
            pair_phase: PairPhase::Idle,
            pair_peer: String::new(),
            pair_error: String::new(),
            ever_paired: false,
            pair_gen: 0,
            pair_key: String::new(),
            pair_stop: None,
            pair_pending: None,
        }
    }

    /// 配对门禁：配上过就放行（一次性握手），否则拦住第一步。
    fn pairing_done(&self) -> bool {
        self.pair_phase == PairPhase::Paired || self.ever_paired
    }

    /// 关掉守护线程。放这里是为了「取消 / 关闭 / 换配置」都只调一处。
    fn stop_pairing(&mut self) {
        if let Some(stop) = self.pair_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.pair_key.clear();
    }

    fn to_task(&self) -> Config {
        let mut t = Config {
            id: new_id(),
            name: self.name.trim().to_string(),
            server: compose_server(&self.host, &self.port),
            // 密钥留空 = 跟随房间码（与插件同一语义）。这里要**真的写进去**：
            // Config::issues() 要求 token 非空，留空会让「创建任务」永远点不了。
            token: String::new(),
            // 房间码不再兜底成 "default"：加入端没等到对端把配置带过来时，
            // 静默用 default 会建出一个连不上任何人的任务，不如直接拦住并说明。
            workspace: self.workspace.trim().to_string(),
            root: self.root.trim().to_string(),
            mode: self.mode,
            filter: Filter {
                ignore_dotfiles: self.dotfiles,
                ignore_extensions: self
                    .ext
                    .split(',')
                    .map(|s| s.trim().trim_start_matches('.').to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                size_min: None,
                size_max: None,
            },
            paused: false,
        };
        if t.token.trim().is_empty() {
            t.token = t.workspace.clone();
        }
        if t.name.is_empty() {
            t.name = t.display_name();
        }
        t
    }
}

const MB: f64 = 1024.0 * 1024.0;

/// 1234567 → 「1.2 MB」；进度条和速率都用它，别在两处各写一套。
fn human_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f >= K * K * K {
        format!("{:.2} GB", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.1} MB", f / (K * K))
    } else if f >= K {
        format!("{:.0} KB", f / K)
    } else {
        format!("{n} B")
    }
}

fn main() -> eframe::Result<()> {
    // 只查图形后端，不开窗口：服务器上「起不来」时，先跑这个把清单拿到手
    if std::env::args().any(|a| a == "--check-render") {
        check_render();
        return Ok(());
    }

    // 「换后端重试」的子进程不要抢单实例锁：父进程这时可能还没退干净
    let retried = std::env::var("WINSYNC_RENDERER_RETRY").is_ok();

    #[cfg(windows)]
    if !retried && !acquire_single_instance() {
        // 已经有实例在跑：把它的窗口叫到前台，然后自己悄悄退出。
        // 弹「已经在运行了」再让用户自己去托盘找，是双开用户最讨厌的体验。
        if !show_main_window() {
            // 极端情况：进程还在但窗口没建出来（正在启动/卡住），才提示。
            rfd::MessageDialog::new()
                .set_title("WinSync")
                .set_description("WinSync 已经在运行了。\n请在任务栏或托盘里找到已有的窗口。")
                .set_level(rfd::MessageLevel::Info)
                .show();
        }
        return Ok(());
    }

    spawn_tray();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1080.0, 760.0])
        .with_min_inner_size([820.0, 560.0])
        .with_title(window_title());

    if let Some((w, h, rgba)) = app_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(egui::IconData {
            rgba,
            width: w,
            height: h,
        }));
    }

    // 默认走 DirectX 12（wgpu）：Windows Server / RDP 里没有 OpenGL 2.0 的 ICD，
    // glow 建上下文会直接失败、进程启动即崩；wgpu 还能退到 WARP 软件光栅。
    // 想强制换：set WINSYNC_RENDERER=glow 或 wgpu
    let want = std::env::var("WINSYNC_RENDERER")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let first = if want == "glow" {
        eframe::Renderer::Glow
    } else {
        eframe::Renderer::Wgpu
    };
    let other = if want == "glow" { "wgpu" } else { "glow" };

    match launch(first, viewport) {
        Ok(()) => Ok(()),
        Err(err) => {
            // 先把自己的失败记下来：换子进程重试后，这一条不会被覆盖掉
            append_gui_log(&format!(
                "{} 启动失败：后端={} 错误={}",
                now_stamp(),
                backend_name(first),
                one_line(&err)
            ));
            // 同一个进程里换不了（winit 的事件循环只能建一次），交给子进程重来；
            // 把这次的错误也带过去，否则最后只剩子进程那条，看不到全貌
            if !retried && relaunch_with(other, &err) {
                return Ok(());
            }
            let prev = std::env::var("WINSYNC_RENDER_PREV").ok();
            report_render_failure(&err, first, prev.as_deref());
            std::process::exit(1);
        }
    }
}

fn backend_name(renderer: eframe::Renderer) -> &'static str {
    match renderer {
        eframe::Renderer::Glow => "OpenGL(glow)",
        _ => "DirectX12(wgpu)",
    }
}

/// `--check-render`：枚举这块机器上 wgpu 能看见的图形适配器，写日志 + 弹框。
///
/// 不开窗口、不建上下文，所以「没有 OpenGL 的服务器」上一定能跑。
/// 服务器起不来时先要这份清单：有没有 DX12、有没有 WARP（`DeviceType::Cpu`）一目了然。
fn check_render() {
    use eframe::wgpu::{Backends, Instance, InstanceDescriptor};

    let mut lines: Vec<String> = Vec::new();
    for backends in [Backends::DX12, Backends::VULKAN, Backends::GL, Backends::all()] {
        let label = format!("{backends:?}");
        let instance = Instance::new(InstanceDescriptor {
            backends,
            ..Default::default()
        });
        let adapters: Vec<String> = instance
            .enumerate_adapters(backends)
            .into_iter()
            .map(|adapter| {
                let info = adapter.get_info();
                format!(
                    "{:?}/{:?} name={} driver={} {}",
                    info.backend, info.device_type, info.name, info.driver, info.driver_info
                )
            })
            .collect();
        if adapters.is_empty() {
            lines.push(format!("[{label}] 无适配器"));
        } else {
            for a in adapters {
                lines.push(format!("[{label}] {a}"));
            }
        }
    }

    let report = format!(
        "{} --check-render（wgpu 枚举结果）\n{}\n",
        now_stamp(),
        lines.join("\n")
    );
    append_gui_log(&report);
    eprintln!("{report}");
    rfd::MessageDialog::new()
        .set_title("WinSync 图形后端检查")
        .set_description(format!(
            "{}\n结果已追加到 %LOCALAPPDATA%\\WinSync\\gui.log\n\n\
             需要 DX12（没有显卡时会是 Microsoft Basic Render Driver，即 WARP）。",
            report
        ))
        .set_level(rfd::MessageLevel::Info)
        .show();
}

/// 错误里可能有换行，日志/环境变量只要一行
fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ").trim().to_string()
}

/// wgpu 的后端配置。
///
/// **Windows 上刻意不包含 GL**：Server 2019 / 远程桌面里那套「GDI 通用」的 OpenGL 1.1
/// 会被 wgpu 枚举成一个「可用」适配器（`device_type = Other`，排序还排在 WARP 的 `Cpu` 之前），
/// 选中之后建上下文一样失败——那就等于绕一圈又踩回 glow 的坑（现场日志就是这个现象：
/// wgpu 那次没成，退到 glow 报 `requires opengl 2.0+`）。只留 DX12 / Vulkan：
/// 有显卡的机器一定有其中之一，没显卡的机器由 DX12 的 WARP 软件光栅兜住。
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    let mut config = eframe::egui_wgpu::WgpuConfiguration::default();
    #[cfg(windows)]
    {
        config.supported_backends = eframe::wgpu::Backends::DX12 | eframe::wgpu::Backends::VULKAN;
    }
    #[cfg(not(windows))]
    {
        config.supported_backends = eframe::wgpu::Backends::VULKAN | eframe::wgpu::Backends::GL;
    }
    config.power_preference = eframe::wgpu::PowerPreference::LowPower;
    // 逃生口：想单独验某个后端（比如在服务器上强制走 DX12/WARP）：
    //   set WINSYNC_WGPU_BACKEND=dx12   /  vulkan  /  gl
    if let Ok(want) = std::env::var("WINSYNC_WGPU_BACKEND") {
        let want = want.to_ascii_lowercase();
        let mut picked = eframe::wgpu::Backends::empty();
        if want.contains("dx12") {
            picked |= eframe::wgpu::Backends::DX12;
        }
        if want.contains("vulkan") {
            picked |= eframe::wgpu::Backends::VULKAN;
        }
        if want.contains("gl") {
            picked |= eframe::wgpu::Backends::GL;
        }
        if !picked.is_empty() {
            config.supported_backends = picked;
        }
    }
    config
}

/// 用一个后端起窗口。渲染后端初始化失败时 eframe 有的返回 Err、有的直接 panic，
/// 两种都收下来，交给上层决定「换后端再试」还是「报错」。
fn launch(renderer: eframe::Renderer, viewport: egui::ViewportBuilder) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport,
        renderer,
        wgpu_options: wgpu_options(),
        ..Default::default()
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        eframe::run_native(
            "WinSync",
            options,
            Box::new(|cc| {
                note_render_backend(cc);
                setup_fonts(&cc.egui_ctx);
                setup_style(&cc.egui_ctx);
                Ok(Box::new(App::new()))
            }),
        )
    }));
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(payload) => Err(panic_text(payload)),
    }
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "未知错误".to_string()
    }
}

/// 换一个后端再试：**必须开新进程**。winit 的事件循环一个进程只能建一次，
/// 在同一个进程里重试会直接 panic，所以让子进程带着环境变量重来，自己先退出。
#[cfg(windows)]
fn relaunch_with(renderer: &str, reason: &str) -> bool {
    match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe)
            .env("WINSYNC_RENDERER", renderer)
            .env("WINSYNC_RENDERER_RETRY", "1")
            .env("WINSYNC_RENDER_PREV", one_line(reason))
            .spawn()
            .is_ok(),
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn relaunch_with(_renderer: &str, _reason: &str) -> bool {
    false
}

/// 每次启动记一行「到底用的是哪个图形后端」。
///
/// 服务器上窗口起不来时，第一个要看的不是错误码而是这一行：
/// 走的是硬件 DX12、还是 WARP 软件光栅（`device_type=Cpu`）、还是 GL。
fn note_render_backend(cc: &eframe::CreationContext<'_>) {
    let line = match &cc.wgpu_render_state {
        Some(state) => {
            let info = state.adapter.get_info();
            format!(
                "{} 启动：后端=wgpu/{:?} 设备={:?} 适配器={}",
                now_stamp(),
                info.backend,
                info.device_type,
                info.name
            )
        }
        None => format!("{} 启动：后端=glow(OpenGL)", now_stamp()),
    };
    append_gui_log(&line);
}

/// 往 gui.log 追加一行，只留最近 200 行。
/// 启动成功记「用的是哪个后端」，起不来记「哪个后端因为什么失败」——
/// 服务器上排查，这两类才是要看的东西。
fn append_gui_log(line: &str) {
    let mut dir = std::env::var("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    dir.push("WinSync");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("gui.log");
    let mut kept: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(|s| s.to_string())
        .collect();
    kept.push(line.to_string());
    if kept.len() > 200 {
        kept.drain(0..kept.len() - 200);
    }
    let _ = std::fs::write(&path, kept.join("\n") + "\n");
}

/// 两个后端都起不来：写一份能看的日志，再弹框说清怎么办（而不是静默退出）。
fn report_render_failure(error: &str, renderer: eframe::Renderer, previous: Option<&str>) {
    let backend = backend_name(renderer);
    let mut dir = std::env::var("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    dir.push("WinSync");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("gui-error.log");
    // 两次尝试都写进去：只看最后一条会把「为什么换的后端」丢掉（现场那次就只剩 glow 的报错）
    let prev_line = match previous {
        Some(p) if !p.is_empty() => format!("上一次：{p}\n"),
        _ => String::new(),
    };
    let body = format!(
        "时间：{stamp}\n后端：{backend}\n错误：{error}\n{prev_line}\n\
         这个窗口起不来，通常是图形后端不可用：\n\
         · Server 2019 / 远程桌面：需要 DirectX 12（没显卡时会走 WARP 软件光栅）。\n\
           若 gui.log 里显示 wgpu 也失败，多半是这台机器连 DX12 都没有，\n\
           或者系统缺少 d3d12.dll / 被组策略禁用了 WARP。\n\
         · 有显卡驱动的机器可以试：set WINSYNC_RENDERER=glow  后再启动；\n\
         · 仍然起不来就用命令行版 winsync.exe 同步（同一套同步引擎，不需要图形界面）。\n\
         日志位置：{path}\n",
        stamp = now_stamp(),
        path = path.display(),
    );
    let _ = std::fs::write(&path, &body);
    append_gui_log(&format!(
        "{} 两个后端都失败：{} / {}",
        now_stamp(),
        one_line(previous.unwrap_or("")),
        one_line(error)
    ));
    rfd::MessageDialog::new()
        .set_title("WinSync 启动失败")
        .set_description(format!(
            "窗口起不来：{error}\n\n两个渲染后端都不可用。\n详情已写入：{}",
            path.display()
        ))
        .set_level(rfd::MessageLevel::Error)
        .show();
    eprintln!("{body}");
}

#[cfg(windows)]
fn acquire_single_instance() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    unsafe {
        let name: Vec<u16> = "Local\\WinSync_SingleInstance_v1\0".encode_utf16().collect();
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if (handle as isize) == 0 {
            return true;
        }
        GetLastError() != ERROR_ALREADY_EXISTS
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in [
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\deng.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\simsun.ttc",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".to_owned(), egui::FontData::from_owned(bytes).into());
            if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                list.insert(0, "cjk".to_owned());
            }
            if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                list.push("cjk".to_owned());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn setup_style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = ui_colors::APP;
    v.window_fill = ui_colors::CARD;
    v.extreme_bg_color = ui_colors::CARD;
    v.faint_bg_color = ui_colors::CARD;
    v.override_text_color = Some(ui_colors::TEXT);
    v.hyperlink_color = ui_colors::PRIMARY;
    v.selection.bg_fill = ui_colors::PRIMARY_DIM;
    v.widgets.noninteractive.bg_fill = ui_colors::CARD;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, ui_colors::BORDER);
    v.widgets.inactive.bg_fill = ui_colors::CARD_HOVER;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, ui_colors::BORDER);
    v.widgets.hovered.bg_fill = ui_colors::CARD_HOVER;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, ui_colors::PRIMARY);
    v.widgets.active.bg_fill = ui_colors::PRIMARY_DIM;
    ctx.set_visuals(v);
}

// ---------------- 托盘 ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    SyncNow,
    ToggleAuto,
    Quit,
}

static TRAY_QUEUE: Mutex<Vec<TrayAction>> = Mutex::new(Vec::new());
static TRAY_READY: AtomicBool = AtomicBool::new(false);
static CTX: OnceLock<egui::Context> = OnceLock::new();

fn tray_ready() -> bool {
    TRAY_READY.load(Ordering::Relaxed)
}

fn drain_tray_actions() -> Vec<TrayAction> {
    TRAY_QUEUE
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

fn push_action(a: TrayAction) {
    if let Ok(mut q) = TRAY_QUEUE.lock() {
        q.push(a);
    }
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
    if matches!(a, TrayAction::Open) {
        show_main_window();
    }
}

fn window_title() -> String {
    format!("WinSync v{} — 文件夹同步", env!("CARGO_PKG_VERSION"))
}

fn show_main_window() -> bool {
    if let Some(ctx) = CTX.get() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
        };
        let wide: Vec<u16> = window_title()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hwnd = FindWindowW(std::ptr::null(), wide.as_ptr());
        if !hwnd.is_null() {
            // 窗口可能在托盘里（SW_HIDE），先显式显示再还原，只发 SW_RESTORE 对
            // 隐藏窗口不生效。
            ShowWindow(hwnd, SW_SHOW);
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
            return true;
        }
    }
    false
}

fn spawn_tray() {
    #[cfg(windows)]
    std::thread::spawn(|| {
        use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};

        let open = MenuItem::with_id("open", "打开主界面", true, None);
        let sync = MenuItem::with_id("sync", "全部立即同步", true, None);
        let auto = MenuItem::with_id("auto", "暂停 / 恢复自动同步", true, None);
        let quit = MenuItem::with_id("quit", "退出", true, None);

        let menu = Menu::new();
        let _ = menu.append(&open);
        let _ = menu.append(&sync);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&auto);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit);

        let Some((w, h, rgba)) = app_icon() else {
            return;
        };
        let Ok(icon) = tray_icon::Icon::from_rgba(rgba, w, h) else {
            return;
        };
        let Ok(_tray) = tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("WinSync — 文件夹同步")
            .with_icon(icon)
            .build()
        else {
            log_line("托盘创建失败，按普通窗口程序运行");
            return;
        };

        MenuEvent::set_event_handler(Some(Box::new(|ev: MenuEvent| match ev.id.0.as_str() {
            "open" => push_action(TrayAction::Open),
            "sync" => push_action(TrayAction::SyncNow),
            "auto" => push_action(TrayAction::ToggleAuto),
            "quit" => push_action(TrayAction::Quit),
            _ => {}
        })));

        tray_icon::TrayIconEvent::set_event_handler(Some(Box::new(
            |ev: tray_icon::TrayIconEvent| {
                if matches!(
                    ev,
                    tray_icon::TrayIconEvent::Click { .. }
                        | tray_icon::TrayIconEvent::DoubleClick { .. }
                ) {
                    push_action(TrayAction::Open);
                }
            },
        )));

        TRAY_READY.store(true, Ordering::Relaxed);
        log_line("托盘已就绪：关闭窗口不会退出，点击图标可打开主界面");

        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, GetMessageW, TranslateMessage, MSG,
            };
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
}

fn log_line(s: &str) {
    let dir = Config::dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("sync.log"))
    {
        use std::io::Write;
        let _ = writeln!(f, "[{}] {}", stamp_full(), s);
    }
}

fn app_icon() -> Option<(u32, u32, Vec<u8>)> {
    let bytes: &[u8] = include_bytes!("../../winres/app.ico");
    if bytes.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut best: Option<(usize, usize, usize)> = None;
    for i in 0..count {
        let e = 6 + i * 16;
        if e + 16 > bytes.len() {
            break;
        }
        let w = if bytes[e] == 0 { 256 } else { bytes[e] as usize };
        let len = u32::from_le_bytes(bytes[e + 8..e + 12].try_into().ok()?) as usize;
        let off = u32::from_le_bytes(bytes[e + 12..e + 16].try_into().ok()?) as usize;
        let better = match best {
            None => true,
            Some((bw, _, _)) => {
                (w == 64 && bw != 64)
                    || (w <= 64 && w > bw && bw != 64)
                    || (w > 64 && bw > 64 && w < bw)
            }
        };
        if better && off + len <= bytes.len() {
            best = Some((w, off, len));
        }
    }
    let (w, off, _) = best?;
    let hdr = &bytes[off..];
    if hdr.len() < 40 {
        return None;
    }
    let hdr_size = u32::from_le_bytes(hdr[0..4].try_into().ok()?) as usize;
    let bpp = u16::from_le_bytes(hdr[14..16].try_into().ok()?);
    if bpp != 32 {
        return None;
    }
    let h = w;
    let start = off + hdr_size;
    let mut rgba = vec![0u8; w * h * 4];
    for row in 0..h {
        let src_row = h - 1 - row;
        for col in 0..w {
            let s = start + (src_row * w + col) * 4;
            let d = (row * w + col) * 4;
            if s + 4 > bytes.len() {
                return None;
            }
            rgba[d] = bytes[s + 2];
            rgba[d + 1] = bytes[s + 1];
            rgba[d + 2] = bytes[s];
            rgba[d + 3] = bytes[s + 3];
        }
    }
    Some((w as u32, h as u32, rgba))
}

// ---------------- 开机自启 ----------------

#[cfg(windows)]
fn autostart_get() -> bool {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    };
    unsafe {
        let sub: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
            .encode_utf16()
            .collect();
        let mut key = std::mem::zeroed();
        if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_QUERY_VALUE, &mut key) != 0 {
            return false;
        }
        let name: Vec<u16> = "WinSync\0".encode_utf16().collect();
        let mut ty = 0u32;
        let mut len = 0u32;
        let rc = RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            std::ptr::null_mut(),
            &mut len,
        );
        RegCloseKey(key);
        rc == 0 && len > 0
    }
}

#[cfg(windows)]
fn autostart_set(on: bool) -> bool {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    unsafe {
        let sub: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
            .encode_utf16()
            .collect();
        let mut key = std::mem::zeroed();
        let mut disp = 0u32;
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            &mut disp,
        ) != 0
        {
            return false;
        }
        let name: Vec<u16> = "WinSync\0".encode_utf16().collect();
        let ok = if on {
            let exe = std::env::current_exe().unwrap_or_default();
            let cmd = format!("\"{}\"", exe.display());
            let data: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            ) == 0
        } else {
            RegDeleteValueW(key, name.as_ptr()) == 0
        };
        RegCloseKey(key);
        ok
    }
}

// ---------------- App ----------------

struct App {
    settings: Settings,
    runtimes: Vec<Runtime>,
    selected: usize,
    tab: Tab,
    buf: EditBuf,
    auto: bool,
    wizard: Option<Wizard>,
    task_editor: Option<usize>,

    records: Vec<(String, Activity)>,
    records_loaded: bool,
    records_task: Option<String>,
    records_status: u8, // 0 全部 1 成功 2 失败
    records_query: String,
    records_show_all: bool,

    saved_hint: Option<String>,
    settings_hint: Option<String>,
    show_raw_log: bool,

    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    watch_tx: Sender<String>,
    watch_rx: Receiver<String>,
    /// 配对守护 → 界面（带 gen，过期的丢）
    pair_tx: Sender<(u64, PairEvent)>,
    pair_rx: Receiver<(u64, PairEvent)>,

    tray_ready: bool,
    quitting: bool,
    hidden_notice: bool,
    autostart: bool,
}

impl App {
    fn new() -> Self {
        let (settings, load_err) = match Settings::load() {
            Ok(Some(s)) => (s, None),
            Ok(None) => (Settings::default(), None),
            Err(e) => (
                Settings::default(),
                Some(format!("读取配置失败：{e}\n已退回空配置")),
            ),
        };

        let runtimes = settings.tasks.iter().map(|_| Runtime::new()).collect();
        let buf = settings
            .tasks
            .first()
            .map(EditBuf::from_task)
            .unwrap_or_else(|| EditBuf::from_task(&Config::default()));

        let (tx, rx) = channel();
        let (watch_tx, watch_rx) = channel();
        let (pair_tx, pair_rx) = channel();

        #[cfg(windows)]
        let autostart = autostart_get();
        #[cfg(not(windows))]
        let autostart = false;

        let mut app = Self {
            settings,
            runtimes,
            selected: 0,
            tab: Tab::Tasks,
            buf,
            auto: true,
            wizard: None,
            task_editor: None,
            records: Vec::new(),
            records_loaded: false,
            records_task: None,
            records_status: 0,
            records_query: String::new(),
            records_show_all: false,
            saved_hint: None,
            settings_hint: None,
            show_raw_log: false,
            tx,
            rx,
            watch_tx,
            watch_rx,
            pair_tx,
            pair_rx,
            tray_ready: false,
            quitting: false,
            hidden_notice: false,
            autostart,
        };

        #[cfg(debug_assertions)]
        if std::env::var("WINSYNC_GUI_SELFTEST").is_ok() {
            let mut w = Wizard::new(None);
            w.host = std::env::var("WINSYNC_GUI_HOST").unwrap_or_else(|_| "127.0.0.1".into());
            w.port = std::env::var("WINSYNC_GUI_PORT").unwrap_or_else(|_| "8443".into());
            w.channel = "123456".into();
            app.wizard = Some(w);
            SELFTEST_FRAMES.store(45, Ordering::Relaxed);
        }

        let deadline = Instant::now() + Duration::from_millis(1000);
        while Instant::now() < deadline && !tray_ready() {
            std::thread::sleep(Duration::from_millis(20));
        }
        app.tray_ready = tray_ready();
        if let Some(e) = load_err {
            app.log(0, e);
        }
        app.prune_logs();
        app
    }

    fn selected_task(&self) -> Option<&Config> {
        self.settings.tasks.get(self.selected)
    }

    fn apply_buf(&mut self) {
        if let Some(t) = self.settings.tasks.get_mut(self.selected) {
            self.buf.apply_to(t);
        }
    }

    fn select(&mut self, i: usize) {
        if i >= self.settings.tasks.len() {
            return;
        }
        self.apply_buf();
        self.selected = i;
        self.buf = EditBuf::from_task(&self.settings.tasks[i]);
        self.records_loaded = false;
        self.saved_hint = None;
    }

    fn log(&mut self, idx: usize, s: impl Into<String>) {
        let s = s.into();
        let name = self
            .settings
            .tasks
            .get(idx)
            .map(|t| t.display_name())
            .unwrap_or_else(|| "系统".to_string());
        log_line(&format!("[{name}] {s}"));
        if let Some(rt) = self.runtimes.get_mut(idx) {
            rt.logs.push(s);
            if rt.logs.len() > 400 {
                rt.logs.remove(0);
            }
        }
    }

    fn add_task(&mut self, task: Config) {
        self.settings.tasks.push(task);
        self.runtimes.push(Runtime::new());
        let _ = self.settings.save();
        let idx = self.settings.tasks.len() - 1;
        self.select(idx);
        self.tab = Tab::Tasks;
        self.log(idx, "任务已创建");
    }

    fn remove_task(&mut self, i: usize) {
        if i >= self.settings.tasks.len() {
            return;
        }
        let task = self.settings.tasks.remove(i);
        self.runtimes.remove(i);
        let _ = self.settings.save();
        log_line(&format!(
            "已删除任务 {}（状态库保留在 {}）",
            task.display_name(),
            task.state_db().display()
        ));
        if self.selected >= self.settings.tasks.len() {
            self.selected = self.settings.tasks.len().saturating_sub(1);
        }
        if let Some(t) = self.settings.tasks.get(self.selected) {
            self.buf = EditBuf::from_task(t);
        }
        self.records_loaded = false;
    }

    fn toggle_pause(&mut self, i: usize) {
        if let Some(t) = self.settings.tasks.get_mut(i) {
            t.paused = !t.paused;
            let paused = t.paused;
            let _ = self.settings.save();
            self.log(
                i,
                if paused {
                    "任务已暂停（不再自动同步，仍可手动「立即同步」）"
                } else {
                    "任务已恢复自动同步"
                },
            );
        }
    }

    fn limits(&self) -> Limits {
        Limits {
            up_kbps: self.settings.limit_up_kbps,
            down_kbps: self.settings.limit_down_kbps,
        }
    }

    fn start_sync(&mut self, i: usize) {
        if i >= self.settings.tasks.len() || self.runtimes[i].running {
            return;
        }
        if i == self.selected {
            self.apply_buf();
        }
        let cfg = self.settings.tasks[i].clone();
        let issues = cfg.issues();
        if !issues.is_empty() {
            self.log(i, format!("无法同步：{}", issues.join("；")));
            return;
        }
        let _ = self.settings.save();

        self.runtimes[i].running = true;
        self.runtimes[i].progress = None;
        self.log(i, format!("开始同步 {}", cfg.root));

        let tx = self.tx.clone();
        let id = cfg.id.clone();
        let limits = self.limits();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(WorkerMsg::Done {
                        task: id,
                        res: Err(format!("运行时创建失败: {e}")),
                    });
                    return;
                }
            };

            let (evt_tx, evt_rx) = channel::<Event>();
            let tx_fwd = tx.clone();
            let id2 = cfg.id.clone();
            let fwd = std::thread::spawn(move || {
                while let Ok(ev) = evt_rx.recv() {
                    if tx_fwd
                        .send(WorkerMsg::Event {
                            task: id2.clone(),
                            ev,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });

            let res = rt.block_on(session::run(&cfg, Some(evt_tx), true, true, 25, limits));
            let _ = fwd.join();
            let _ = tx.send(WorkerMsg::Done {
                task: cfg.id.clone(),
                res: res.map_err(|e| format!("{e:#}")),
            });
        });
    }

    /// 按需起/重起配对守护：地址填好、通道码满 6 位就自动开始（与插件一致）。
    /// 「配置指纹」变了才重开，否则每帧都会起一条新线程。
    fn sync_pairing(&mut self) {
        let Some(w) = self.wizard.as_mut() else {
            return;
        };
        if w.step != 0 {
            return;
        }
        let channel = w.channel.trim().to_string();
        if channel.len() != 6 || !channel.chars().all(|c| c.is_ascii_digit()) {
            return;
        }
        if w.host.trim().is_empty() {
            return;
        }
        let server = compose_server(&w.host, &w.port);
        let key = format!("{}|{}|{}", channel, server, w.entry);
        if key == w.pair_key {
            w.pair_pending = None;
            return;
        }
        // 每敲一个字符 key 都会变：先记下时间，安静 500ms 再起线程
        match w.pair_pending {
            None => {
                w.pair_pending = Some(Instant::now());
                return;
            }
            Some(at) if at.elapsed() < Duration::from_millis(500) => return,
            Some(_) => {}
        }
        w.pair_pending = None;
        w.stop_pairing();
        w.pair_gen += 1;
        w.pair_key = key;
        w.pair_phase = PairPhase::Connecting;
        w.pair_peer.clear();
        w.pair_error.clear();

        // 创建端把「房间码|密钥」挂在 device id 上带过去；加入端只收不发
        let announce = if w.entry == 0 && !w.workspace.trim().is_empty() {
            let room = w.workspace.trim();
            let token = if w.token.trim().is_empty() {
                room
            } else {
                w.token.trim()
            };
            format!("{room}|{token}")
        } else {
            String::new()
        };
        let device = if announce.is_empty() {
            format!("gui-{}", uuid::Uuid::new_v4().simple())
        } else {
            format!("gui-{}~{}", uuid::Uuid::new_v4().simple(), hex_of(&announce))
        };

        let stop = Arc::new(AtomicBool::new(false));
        w.pair_stop = Some(stop.clone());
        spawn_pairing(
            w.pair_gen,
            server,
            channel,
            device,
            self.pair_tx.clone(),
            stop,
        );
    }

    fn task_index(&self, id: &str) -> Option<usize> {
        self.settings.tasks.iter().position(|t| t.id == id)
    }

    /// 按设置清理历史记录：先按天数，再按条数。
    fn prune_logs(&mut self) {
        let days = self.settings.log_keep_days;
        let keep = self.settings.log_keep_count;
        if days == 0 && keep == 0 {
            return;
        }
        for t in &self.settings.tasks {
            if let Ok(store) = Store::open(&t.state_db()) {
                if days > 0 {
                    let cutoff = now_secs() - days as i64 * 86400;
                    let _ = store.prune_activities_before(cutoff);
                }
                if keep > 0 {
                    let _ = store.prune_activities(keep as usize);
                }
            }
        }
    }

    fn reload_records(&mut self) {
        self.records.clear();
        let ids: Vec<String> = if self.records_show_all {
            self.settings.tasks.iter().map(|t| t.id.clone()).collect()
        } else if let Some(id) = &self.records_task {
            vec![id.clone()]
        } else {
            self.selected_task()
                .map(|t| vec![t.id.clone()])
                .unwrap_or_default()
        };

        for id in ids {
            if let Some(i) = self.task_index(&id) {
                let task = self.settings.tasks[i].clone();
                if let Ok(store) = Store::open(&task.state_db()) {
                    if let Ok(list) = store.recent_activities(300) {
                        for a in list {
                            self.records.push((task.display_name(), a));
                        }
                    }
                }
            }
        }
        self.records.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));
        self.records.truncate(500);
        self.records_loaded = true;
    }

    fn export_records(&mut self) {
        let path = Config::dir().join("records.csv");
        let mut out = String::from("time,task,file,event,status,message\n");
        for (task, a) in &self.records {
            out.push_str(&format!(
                "{},{},{},{},{},{}\n",
                fmt_ts_full(a.created_at),
                csv_escape(task),
                csv_escape(&a.path),
                a.action,
                if a.ok { "成功" } else { "失败" },
                csv_escape(&a.message)
            ));
        }
        match std::fs::write(&path, out) {
            Ok(()) => {
                self.settings_hint = Some(format!("已导出到 {}", path.display()));
                let _ = std::process::Command::new("explorer")
                    .arg(Config::dir())
                    .spawn();
            }
            Err(e) => self.settings_hint = Some(format!("导出失败: {e}")),
        }
    }
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let _ = CTX.set(ctx.clone());

        for action in drain_tray_actions() {
            match action {
                TrayAction::Open => {}
                TrayAction::SyncNow => {
                    for i in 0..self.settings.tasks.len() {
                        self.start_sync(i);
                    }
                }
                TrayAction::ToggleAuto => {
                    self.auto = !self.auto;
                    self.log(
                        0,
                        if self.auto {
                            "已恢复自动同步"
                        } else {
                            "已暂停自动同步"
                        },
                    );
                }
                TrayAction::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.tray_ready {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            if !self.hidden_notice {
                self.log(0, "已隐藏到托盘，同步继续运行（点托盘图标打开）");
                self.hidden_notice = true;
            }
        }

        // 配对守护发来的阶段变化：只认当前这一代，旧线程的事件直接丢
        while let Ok((gen, ev)) = self.pair_rx.try_recv() {
            if let Some(w) = &mut self.wizard {
                if w.pair_gen != gen {
                    continue;
                }
                match ev {
                    PairEvent::Phase(phase, peer, error) => {
                        // 配上过就不再回退：对端离开配对房间不该把门禁重新关上
                        if phase == PairPhase::Paired {
                            w.ever_paired = true;
                        }
                        w.pair_phase = phase;
                        w.pair_peer = peer;
                        w.pair_error = error;
                    }
                    PairEvent::Carried(room, token) => {
                        // 加入端：创建端把房间码与密钥带过来了，直接填上（默认仍是打码显示）
                        if w.entry == 1 && !room.is_empty() {
                            w.workspace = room;
                            w.token = token;
                        }
                    }
                }
            }
        }
        self.sync_pairing();
        #[cfg(debug_assertions)]
        if SELFTEST_FRAMES.load(Ordering::Relaxed) > 0 {
            if SELFTEST_FRAMES.fetch_sub(1, Ordering::Relaxed) == 1 {
                let phase = self.wizard.as_ref().map(|w| w.pair_phase);
                std::eprintln!("[selftest] 向导画完 45 帧，配对阶段={:?}，退出", phase);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        // 向导开着就保持重绘：配对状态由线程推进，不重绘界面就不会更新
        if self.wizard.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Event { task, ev } => {
                    if let Some(i) = self.task_index(&task) {
                        match ev {
                            Event::Log(s) => {
                                if let Some(rt) = self.runtimes.get_mut(i) {
                                    rt.logs.push(s);
                                    if rt.logs.len() > 400 {
                                        rt.logs.remove(0);
                                    }
                                }
                            }
                            Event::Progress { done, total, what, up_bps, down_bps } => {
                                if let Some(rt) = self.runtimes.get_mut(i) {
                                    rt.progress = Some((done, total, what, up_bps, down_bps));
                                }
                            }
                        }
                    }
                }
                WorkerMsg::Done { task, res } => {
                    if let Some(i) = self.task_index(&task) {
                        let summary = match &res {
                            Ok(r) if r.up_to_date => "两端已一致，无需传输".to_string(),
                            Ok(r) => format!(
                                "完成：推送 {}，拉取 {}，删除 {}，冲突 {}",
                                r.pushed, r.pulled, r.deleted, r.conflicts
                            ),
                            Err(e) => format!("失败：{e}"),
                        };
                        if let Some(rt) = self.runtimes.get_mut(i) {
                            rt.running = false;
                            rt.progress = None;
                            rt.last_time = Some(now_stamp());
                            rt.dirty = false;
                            rt.last = Some(res);
                        }
                        self.log(i, summary);
                        if self.records_loaded {
                            self.reload_records();
                        }
                    }
                }
            }
        }

        let watch_events: Vec<String> = {
            let mut v = Vec::new();
            while let Ok(id) = self.watch_rx.try_recv() {
                v.push(id);
            }
            v
        };
        for id in watch_events {
            if let Some(i) = self.task_index(&id) {
                self.runtimes[i].dirty = true;
                self.runtimes[i].last_change = Instant::now();
            }
        }

        let auto = self.auto;
        let mut to_start: Vec<usize> = Vec::new();
        for i in 0..self.settings.tasks.len() {
            let cfg = self.settings.tasks[i].clone();
            let rt = &mut self.runtimes[i];

            if rt.watch_root != cfg.root {
                rt.watcher = None;
                rt.watch_root.clear();
                let path = std::path::PathBuf::from(cfg.root.trim());
                if path.is_dir() {
                    let tx = self.watch_tx.clone();
                    let id = cfg.id.clone();
                    if let Ok(mut w) = notify::recommended_watcher(
                        move |res: notify::Result<notify::Event>| {
                            if let Ok(ev) = res {
                                if !matches!(ev.kind, notify::EventKind::Access(_)) {
                                    let _ = tx.send(id.clone());
                                }
                            }
                        },
                    ) {
                        if w.watch(&path, notify::RecursiveMode::Recursive).is_ok() {
                            rt.watcher = Some(w);
                            rt.watch_root = cfg.root.clone();
                        }
                    }
                }
            }

            if !auto || cfg.paused || rt.running {
                continue;
            }
            let due_poll = Instant::now() >= rt.next_auto;
            let due_change = rt.dirty && rt.last_change.elapsed() > Duration::from_secs(3);
            if due_poll || due_change {
                rt.next_auto = Instant::now() + Duration::from_secs(28 + jitter());
                rt.dirty = false;
                if cfg.issues().is_empty() {
                    to_start.push(i);
                }
            }
        }
        for i in to_start {
            self.start_sync(i);
        }

        if self.tab == Tab::Log && !self.records_loaded {
            self.reload_records();
        }

        self.ui_sidebar(ctx);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(ui_colors::APP)
                    .inner_margin(egui::Margin::symmetric(24.0, 16.0)),
            )
            .show(ctx, |ui| {
                if self.wizard.is_some() {
                    self.ui_wizard(ui);
                } else {
                    match self.tab {
                        Tab::Tasks => self.ui_tasks(ui),
                        Tab::Log => self.ui_log(ui),
                        Tab::Settings => self.ui_settings(ui),
                    }
                }
            });

        if self.show_raw_log {
            self.ui_raw_log(ctx);
        }
        self.ui_task_editor(ctx);

        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

impl App {
    fn ui_sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("nav")
            .exact_width(180.0)
            .resizable(false)
            .frame(
                egui::Frame::none()
                    .fill(ui_colors::SIDEBAR)
                    .inner_margin(egui::Margin::symmetric(12.0, 14.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
                    ui.painter()
                        .rect_filled(rect, 8.0, ui_colors::PRIMARY);
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "W",
                        egui::FontId::proportional(15.0),
                        egui::Color32::WHITE,
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("WinSync")
                            .size(15.0)
                            .strong()
                            .color(ui_colors::TEXT),
                    );
                });

                ui.add_space(18.0);
                if nav_item(ui, self.tab == Tab::Tasks, "同步任务") {
                    self.tab = Tab::Tasks;
                }
                ui.add_space(2.0);
                if nav_item(ui, self.tab == Tab::Log, "同步日志") {
                    self.tab = Tab::Log;
                    self.records_loaded = false;
                }
                ui.add_space(2.0);
                if nav_item(ui, self.tab == Tab::Settings, "设置") {
                    self.tab = Tab::Settings;
                }

                // 底部「创建同步任务」
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.add_space(4.0);
                    if nav_item(ui, false, "+ 创建同步任务") {
                        let seed = self.settings.tasks.first().cloned();
                        self.wizard = Some(Wizard::new(seed.as_ref()));
                    }
                });
            });
    }

    fn ui_tasks(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("同步任务")
                    .size(18.0)
                    .strong()
                    .color(ui_colors::TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let btn = egui::Button::new(
                    egui::RichText::new("+ 创建同步任务")
                        .size(12.0)
                        .color(egui::Color32::WHITE),
                )
                .fill(ui_colors::PRIMARY)
                .rounding(egui::Rounding::same(6.0))
                .min_size(egui::vec2(120.0, 30.0));
                if ui.add(btn).clicked() {
                    let seed = self.settings.tasks.first().cloned();
                    self.wizard = Some(Wizard::new(seed.as_ref()));
                }
                ui.checkbox(&mut self.auto, egui::RichText::new("自动同步").size(12.0));
            });
        });

        ui.add_space(14.0);

        // 设备行（对应飞牛的 NAS 行）
        if let Some(t) = self.selected_task() {
            let host = short_server(&t.server);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "🖥  {}  |  {}  ·  房间 {}",
                        computer_name(),
                        host,
                        short_room(&t.workspace)
                    ))
                        .size(12.0)
                        .color(ui_colors::TEXT_2),
                );
            });
            ui.add_space(10.0);
        }

        if self.settings.tasks.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("还没有同步任务")
                        .size(14.0)
                        .color(ui_colors::TEXT),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("点右上角「+ 创建同步任务」，三步即可开始")
                        .size(12.0)
                        .color(ui_colors::TEXT_3),
                );
            });
            return;
        }

        let mut select: Option<usize> = None;
        let mut pause: Option<usize> = None;
        let mut run: Option<usize> = None;
        let mut remove: Option<usize> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for i in 0..self.settings.tasks.len() {
                    let task = self.settings.tasks[i].clone();
                    let selected = i == self.selected;
                    let (running, progress, last, last_time) = {
                        let rt = &self.runtimes[i];
                        (
                            rt.running,
                            rt.progress.clone(),
                            rt.last.clone(),
                            rt.last_time.clone(),
                        )
                    };

                    let frame = egui::Frame::none()
                        .fill(ui_colors::CARD)
                        .stroke(egui::Stroke::new(
                            if selected { 1.0_f32 } else { 0.5_f32 },
                            if selected {
                                ui_colors::PRIMARY
                            } else {
                                ui_colors::BORDER
                            },
                        ))
                        .rounding(egui::Rounding::same(10.0))
                        .inner_margin(egui::Margin::same(16.0));

                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            draw_folder(ui);
                            ui.add_space(10.0);
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(task.display_name())
                                        .size(15.0)
                                        .strong()
                                        .color(ui_colors::TEXT),
                                );
                                let (color, text) = if running {
                                    (ui_colors::WARN, "同步中…")
                                } else if task.paused {
                                    (ui_colors::TEXT_3, "已暂停")
                                } else if matches!(last, Some(Err(_))) {
                                    (ui_colors::DANGER, "同步失败")
                                } else if last.is_some() {
                                    (ui_colors::SUCCESS, "同步完成")
                                } else {
                                    (ui_colors::TEXT_3, "等待同步")
                                };
                                ui.label(egui::RichText::new(text).size(12.0).color(color));
                            });

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.menu_button("⋯", |ui| {
                                        if ui.button("设置").clicked() {
                                            select = Some(i);
                                            ui.close_menu();
                                        }
                                        if ui.button("打开目录").clicked() {
                                            let _ = std::process::Command::new("explorer")
                                                .arg(&task.root)
                                                .spawn();
                                            ui.close_menu();
                                        }
                                        ui.separator();
                                        if ui.button("删除任务").clicked() {
                                            remove = Some(i);
                                            ui.close_menu();
                                        }
                                    });
                                    if pause_button(ui, task.paused) {
                                        pause = Some(i);
                                    }
                                },
                            );
                        });

                        if let Some((done, total, what, up_bps, down_bps)) = &progress {
                            ui.add_space(8.0);
                            let frac = if *total == 0 {
                                0.0
                            } else {
                                (*done as f32 / *total as f32).clamp(0.0, 1.0)
                            };
                            ui.add(
                                egui::ProgressBar::new(frac)
                                    .show_percentage()
                                    .text(format!("{} / {}", human_bytes(*done), human_bytes(*total))),
                            );
                            let mut line = what.clone();
                            if *up_bps > 0 {
                                line.push_str(&format!("　↑ {}/s", human_bytes(*up_bps)));
                            }
                            if *down_bps > 0 {
                                line.push_str(&format!("　↓ {}/s", human_bytes(*down_bps)));
                            }
                            ui.label(egui::RichText::new(line).size(11.0).color(ui_colors::TEXT_3));
                        }

                        ui.add_space(12.0);
                        ui.painter().hline(
                            ui.available_rect_before_wrap().x_range(),
                            ui.cursor().top(),
                            egui::Stroke::new(0.5_f32, ui_colors::BORDER),
                        );
                        ui.add_space(10.0);

                        // 三列：电脑文件路径 / 目标 / 规则设置（对齐飞牛的三列信息区）
                        let w = ui.available_width();
                        ui.horizontal(|ui| {
                            let col = w / 3.0 - 12.0;
                            ui.allocate_ui(egui::vec2(col, 40.0), |ui| {
                                ui.label(
                                    egui::RichText::new("电脑文件路径")
                                        .size(11.0)
                                        .color(ui_colors::TEXT_3),
                                );
                                ui.label(
                                    egui::RichText::new(if task.root.is_empty() {
                                        "—"
                                    } else {
                                        &task.root
                                    })
                                    .size(12.0)
                                    .color(ui_colors::TEXT),
                                );
                            });
                            ui.allocate_ui(egui::vec2(col, 40.0), |ui| {
                                ui.label(
                                    egui::RichText::new("目标房间")
                                        .size(11.0)
                                        .color(ui_colors::TEXT_3),
                                );
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} / {}",
                                        short_server(&task.server),
                                        short_room(&task.workspace)
                                    ))
                                    .size(12.0)
                                    .color(ui_colors::TEXT),
                                );
                            });
                            ui.allocate_ui(egui::vec2(col, 40.0), |ui| {
                                ui.label(
                                    egui::RichText::new("规则设置")
                                        .size(11.0)
                                        .color(ui_colors::TEXT_3),
                                );
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} · {}",
                                        mode_short(task.mode),
                                        filter_summary(&task.filter)
                                    ))
                                    .size(12.0)
                                    .color(ui_colors::TEXT),
                                );
                            });
                        });

                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            let can = !running && task.issues().is_empty();
                            let btn = egui::Button::new(
                                egui::RichText::new(if running { "同步中…" } else { "立即同步" })
                                    .size(12.0)
                                    .color(egui::Color32::WHITE),
                            )
                            .fill(ui_colors::PRIMARY)
                            .rounding(egui::Rounding::same(6.0))
                            .min_size(egui::vec2(88.0, 28.0));
                            if ui.add_enabled(can, btn).clicked() {
                                run = Some(i);
                            }
                            if let Some(t) = &last_time {
                                ui.label(
                                    egui::RichText::new(format!("上次同步 {t}"))
                                        .size(11.0)
                                        .color(ui_colors::TEXT_3),
                                );
                            }
                            if let Some(Ok(r)) = &last {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "推送 {} · 拉取 {} · 冲突 {}",
                                        r.pushed, r.pulled, r.conflicts
                                    ))
                                    .size(11.0)
                                    .color(ui_colors::TEXT_3),
                                );
                            }
                            if let Some(Err(e)) = &last {
                                ui.label(
                                    egui::RichText::new(e).size(11.0).color(ui_colors::DANGER),
                                );
                            }
                        });
                    });
                    ui.add_space(12.0);
                }
            });

        if let Some(i) = select {
            self.select(i);
            self.task_editor = Some(i);
        }
        if let Some(i) = run {
            self.select(i);
            self.start_sync(i);
        }
        if let Some(i) = pause {
            self.toggle_pause(i);
        }
        if let Some(i) = remove {
            self.remove_task(i);
        }
    }

    /// 同步日志：与飞牛一致的表格（文件名称 / 任务 / 时间 / 状态 / 事件）
    fn ui_log(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("同步日志")
                    .size(18.0)
                    .strong()
                    .color(ui_colors::TEXT),
            );
        });
        ui.add_space(12.0);

        ui.horizontal(|ui| {
            // 任务筛选
            let current = if self.records_show_all {
                "全部任务".to_string()
            } else {
                self.records_task
                    .as_ref()
                    .and_then(|id| self.task_index(id))
                    .and_then(|i| self.settings.tasks.get(i))
                    .map(|t| t.display_name())
                    .unwrap_or_else(|| "当前任务".to_string())
            };
            egui::ComboBox::from_id_salt("rec_task")
                .selected_text(egui::RichText::new(current).size(12.0))
                .width(150.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.records_show_all, "全部任务")
                        .clicked()
                    {
                        self.records_show_all = true;
                        self.records_task = None;
                        self.records_loaded = false;
                    }
                    for t in &self.settings.tasks {
                        let picked = self.records_task.as_deref() == Some(t.id.as_str());
                        if ui.selectable_label(picked, t.display_name()).clicked() {
                            self.records_show_all = false;
                            self.records_task = Some(t.id.clone());
                            self.records_loaded = false;
                        }
                    }
                });

            // 状态筛选
            let status_text = match self.records_status {
                1 => "成功",
                2 => "失败",
                _ => "全部",
            };
            egui::ComboBox::from_id_salt("rec_status")
                .selected_text(egui::RichText::new(status_text).size(12.0))
                .width(100.0)
                .show_ui(ui, |ui| {
                    for (v, label) in [(0u8, "全部"), (1, "成功"), (2, "失败")] {
                        if ui
                            .selectable_label(self.records_status == v, label)
                            .clicked()
                        {
                            self.records_status = v;
                        }
                    }
                });

            // 搜索
            ui.add(
                egui::TextEdit::singleline(&mut self.records_query)
                    .hint_text("搜索文件名称")
                    .desired_width(240.0),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("🗑").on_hover_text("清空当前筛选的记录").clicked() {
                    let ids: Vec<String> = if self.records_show_all {
                        self.settings.tasks.iter().map(|t| t.id.clone()).collect()
                    } else if let Some(id) = &self.records_task {
                        vec![id.clone()]
                    } else {
                        self.selected_task()
                            .map(|t| vec![t.id.clone()])
                            .unwrap_or_default()
                    };
                    for id in ids {
                        if let Some(i) = self.task_index(&id) {
                            if let Ok(store) = Store::open(&self.settings.tasks[i].state_db()) {
                                let _ = store.clear_activities();
                            }
                        }
                    }
                    self.records.clear();
                    self.records_loaded = false;
                }
                if ui.button("导出日志文件").clicked() {
                    self.export_records();
                }
            });
        });

        ui.add_space(10.0);

        // 表头
        let full = ui.available_width();
        ui.horizontal(|ui| {
            let w_name = full * 0.44;
            let w_task = full * 0.14;
            let w_time = full * 0.20;
            let w_status = full * 0.10;
            for (label, w) in [
                ("文件名称", w_name),
                ("任务", w_task),
                ("时间", w_time),
                ("状态", w_status),
                ("事件", full - w_name - w_task - w_time - w_status - 16.0),
            ] {
                ui.allocate_ui(egui::vec2(w, 18.0), |ui| {
                    ui.label(
                        egui::RichText::new(label)
                            .size(12.0)
                            .color(ui_colors::TEXT_2),
                    );
                });
            }
        });
        ui.painter().hline(
            ui.available_rect_before_wrap().x_range(),
            ui.cursor().top(),
            egui::Stroke::new(0.5_f32, ui_colors::BORDER),
        );
        ui.add_space(2.0);

        let q = self.records_query.trim().to_lowercase();
        let rows: Vec<(String, Activity)> = self
            .records
            .iter()
            .filter(|(_, a)| match self.records_status {
                1 => a.ok,
                2 => !a.ok,
                _ => true,
            })
            .filter(|(_, a)| q.is_empty() || a.path.to_lowercase().contains(&q))
            .cloned()
            .collect();

        if rows.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("暂无记录")
                        .size(13.0)
                        .color(ui_colors::TEXT_3),
                );
            });
            return;
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (idx, (task, a)) in rows.iter().enumerate() {
                    if idx % 2 == 1 {
                        let r = ui.available_rect_before_wrap();
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(
                                r.min,
                                egui::vec2(r.width(), 26.0),
                            ),
                            0.0,
                            ui_colors::CARD,
                        );
                    }
                    ui.horizontal(|ui| {
                        let w_name = full * 0.44;
                        let w_task = full * 0.14;
                        let w_time = full * 0.20;
                        let w_status = full * 0.10;
                        ui.allocate_ui(egui::vec2(w_name, 24.0), |ui| {
                            ui.label(
                                egui::RichText::new(&a.path)
                                    .size(12.0)
                                    .color(ui_colors::TEXT),
                            );
                        });
                        ui.allocate_ui(egui::vec2(w_task, 24.0), |ui| {
                            ui.label(
                                egui::RichText::new(task)
                                    .size(12.0)
                                    .color(ui_colors::TEXT_2),
                            );
                        });
                        ui.allocate_ui(egui::vec2(w_time, 24.0), |ui| {
                            ui.label(
                                egui::RichText::new(fmt_ts_full(a.created_at))
                                    .size(12.0)
                                    .color(ui_colors::TEXT_2),
                            );
                        });
                        ui.allocate_ui(egui::vec2(w_status, 24.0), |ui| {
                            ui.label(
                                egui::RichText::new(if a.ok { "成功" } else { "失败" })
                                    .size(12.0)
                                    .color(if a.ok {
                                        ui_colors::SUCCESS
                                    } else {
                                        ui_colors::DANGER
                                    }),
                            );
                        });
                        ui.label(
                            egui::RichText::new(&a.action)
                                .size(12.0)
                                .color(ui_colors::TEXT_2),
                        );
                    });
                }
            });
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new("设置")
                .size(18.0)
                .strong()
                .color(ui_colors::TEXT),
        );
        ui.add_space(14.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // 启动与更新
                ui.horizontal(|ui| {
                    let mut on = self.autostart;
                    if ui
                        .checkbox(&mut on, egui::RichText::new("开机时自动启动").size(13.0))
                        .changed()
                    {
                        #[cfg(windows)]
                        {
                            if autostart_set(on) {
                                self.autostart = on;
                                self.settings_hint = Some(if on {
                                    "已加入开机启动".into()
                                } else {
                                    "已取消开机启动".into()
                                });
                            } else {
                                self.settings_hint = Some("写入注册表失败".into());
                            }
                        }
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let mut v = self.settings.auto_update;
                    ui.add_enabled(
                        false,
                        egui::Checkbox::new(&mut v, egui::RichText::new("有新版本时自动更新").size(13.0)),
                    );
                    ui.label(
                        egui::RichText::new("暂不支持（需更新服务器）")
                            .size(11.0)
                            .color(ui_colors::TEXT_3),
                    );
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "当前版本：v{}",
                            env!("CARGO_PKG_VERSION")
                        ))
                        .size(13.0)
                        .color(ui_colors::TEXT),
                    );
                    if ui.button("检查更新").clicked() {
                        self.settings_hint =
                            Some("当前为本地构建，没有更新服务器".into());
                    }
                });

                section(ui, "同步日志删除规则");
                ui.horizontal(|ui| {
                    let mut on = self.settings.log_keep_count > 0;
                    if ui
                        .checkbox(&mut on, egui::RichText::new("日志保存数量").size(13.0))
                        .changed()
                    {
                        self.settings.log_keep_count = if on { 100_000 } else { 0 };
                        let _ = self.settings.save();
                        self.prune_logs();
                    }
                    let text = match self.settings.log_keep_count {
                        0 => "不限".to_string(),
                        n => format!("{} 万条", n / 10_000),
                    };
                    egui::ComboBox::from_id_salt("keep_count")
                        .selected_text(egui::RichText::new(text).size(12.0))
                        .width(140.0)
                        .show_ui(ui, |ui| {
                            for (v, label) in [
                                (0u32, "不限"),
                                (100_000, "10 万条"),
                                (500_000, "50 万条"),
                                (1_000_000, "100 万条"),
                            ] {
                                if ui
                                    .selectable_label(
                                        self.settings.log_keep_count == v,
                                        label,
                                    )
                                    .clicked()
                                {
                                    self.settings.log_keep_count = v;
                                    let _ = self.settings.save();
                                    self.prune_logs();
                                }
                            }
                        });
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let mut on = self.settings.log_keep_days > 0;
                    if ui
                        .checkbox(&mut on, egui::RichText::new("日志保存时间").size(13.0))
                        .changed()
                    {
                        self.settings.log_keep_days = if on { 30 } else { 0 };
                        let _ = self.settings.save();
                        self.prune_logs();
                    }
                    let text = match self.settings.log_keep_days {
                        0 => "永久".to_string(),
                        d => format!("{d} 天"),
                    };
                    egui::ComboBox::from_id_salt("keep_days")
                        .selected_text(egui::RichText::new(text).size(12.0))
                        .width(140.0)
                        .show_ui(ui, |ui| {
                            for (v, label) in
                                [(0u32, "永久"), (7, "7 天"), (30, "30 天"), (90, "90 天")]
                            {
                                if ui
                                    .selectable_label(self.settings.log_keep_days == v, label)
                                    .clicked()
                                {
                                    self.settings.log_keep_days = v;
                                    let _ = self.settings.save();
                                    self.prune_logs();
                                }
                            }
                        });
                });

                section(ui, "全局限速设置");
                ui.horizontal(|ui| {
                    let mut on = self.settings.limit_up_kbps > 0;
                    if ui
                        .checkbox(&mut on, egui::RichText::new("最大上传速度").size(13.0))
                        .changed()
                    {
                        self.settings.limit_up_kbps = if on { 1024 } else { 0 };
                        let _ = self.settings.save();
                    }
                    let mut v = self.settings.limit_up_kbps as f64;
                    let changed = ui
                        .add_enabled(
                            self.settings.limit_up_kbps > 0,
                            egui::DragValue::new(&mut v)
                                .speed(16.0)
                                .range(64.0..=1_000_000.0)
                                .suffix(" KB/s"),
                        )
                        .changed();
                    if changed {
                        self.settings.limit_up_kbps = v as u32;
                        let _ = self.settings.save();
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let mut on = self.settings.limit_down_kbps > 0;
                    if ui
                        .checkbox(&mut on, egui::RichText::new("最大下载速度").size(13.0))
                        .changed()
                    {
                        self.settings.limit_down_kbps = if on { 1024 } else { 0 };
                        let _ = self.settings.save();
                    }
                    let mut v = self.settings.limit_down_kbps as f64;
                    let changed = ui
                        .add_enabled(
                            self.settings.limit_down_kbps > 0,
                            egui::DragValue::new(&mut v)
                                .speed(16.0)
                                .range(64.0..=1_000_000.0)
                                .suffix(" KB/s"),
                        )
                        .changed();
                    if changed {
                        self.settings.limit_down_kbps = v as u32;
                        let _ = self.settings.save();
                    }
                });
                ui.label(
                    egui::RichText::new("限速在每次同步会话开始时生效")
                        .size(11.0)
                        .color(ui_colors::TEXT_3),
                );

                section(ui, "诊断");
                ui.horizontal(|ui| {
                    if ui.button("导出日志文件").clicked() {
                        self.export_records();
                    }
                    if ui.button("查看运行日志").clicked() {
                        self.show_raw_log = true;
                    }
                    if ui.button("打开数据目录").clicked() {
                        let dir = Config::dir();
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::process::Command::new("explorer").arg(dir).spawn();
                    }
                });

                if let Some(h) = &self.settings_hint {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(h)
                            .size(11.0)
                            .color(ui_colors::TEXT_3),
                    );
                }

                // 当前任务的连接配置
                if self.selected_task().is_some() {
                    section(ui, "当前任务");
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  ·  {}",
                            self.selected_task().map(|t| t.display_name()).unwrap_or_default(),
                            self.selected_task().map(|t| t.root.clone()).unwrap_or_default()
                        ))
                        .size(12.0)
                        .color(ui_colors::TEXT_2),
                    );
                    ui.add_space(6.0);
                    if ui.button("编辑该任务").clicked() {
                        let i = self.selected;
                        self.task_editor = Some(i);
                    }
                    ui.label(
                        egui::RichText::new("任务级配置（服务器、房间、模式、过滤）在任务卡里点「⋯ → 设置」")
                            .size(11.0)
                            .color(ui_colors::TEXT_3),
                    );
                }
            });
    }

    /// 任务设置弹窗：服务器、房间、目录、模式、过滤（配置全部在界面里）
    fn ui_task_editor(&mut self, ctx: &egui::Context) {
        let Some(i) = self.task_editor else {
            return;
        };
        if i >= self.settings.tasks.len() {
            self.task_editor = None;
            return;
        }

        let mut open = true;
        let mut do_pause = false;
        let mut do_delete = false;
        let name = self.settings.tasks[i].display_name();
        let paused = self.settings.tasks[i].paused;

        egui::Window::new(format!("任务设置 · {name}"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(540.0)
            .show(ctx, |ui| {
                field(ui, "任务名", &mut self.buf.name, "留空则用目录名", false);
                ui.add_space(8.0);
                relay_fields(ui, &mut self.buf.host, &mut self.buf.port);
                ui.add_space(8.0);
                room_field(ui, &mut self.buf.workspace, &mut self.buf.reveal_room);
                ui.add_space(8.0);
                field(
                    ui,
                    "密钥",
                    &mut self.buf.token,
                    "留空则与房间码相同（中继配了房间密钥才需要填）",
                    true,
                );
                ui.add_space(8.0);

                ui.label(
                    egui::RichText::new("同步目录")
                        .size(12.0)
                        .color(ui_colors::TEXT_2),
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.buf.root)
                            .hint_text(r"D:/Sync")
                            .desired_width(400.0),
                    );
                    if ui.button("浏览…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.buf.root = dir.to_string_lossy().to_string();
                        }
                    }
                });

                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new("同步模式")
                        .size(13.0)
                        .strong()
                        .color(ui_colors::TEXT_2),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.buf.mode, SyncMode::Bidirectional, "双向");
                    ui.radio_value(&mut self.buf.mode, SyncMode::UploadOnly, "仅上传");
                    ui.radio_value(&mut self.buf.mode, SyncMode::DownloadOnly, "仅下载");
                });

                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new("过滤规则（两端必须一致）")
                        .size(13.0)
                        .strong()
                        .color(ui_colors::TEXT_2),
                );
                ui.add_space(4.0);
                ui.checkbox(
                    &mut self.buf.dotfiles,
                    egui::RichText::new("忽略以 . 开头的文件和目录（如 .git）").size(12.0),
                );
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("忽略扩展名")
                            .size(12.0)
                            .color(ui_colors::TEXT_2),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.buf.ext)
                            .hint_text("lnk, pst, tmp")
                            .desired_width(180.0),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("大小范围")
                            .size(12.0)
                            .color(ui_colors::TEXT_2),
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.buf.min_mb)
                            .speed(0.5)
                            .range(0.0..=1_000_000.0)
                            .suffix(" MB"),
                    );
                    ui.label("—");
                    ui.add(
                        egui::DragValue::new(&mut self.buf.max_mb)
                            .speed(0.5)
                            .range(0.0..=1_000_000.0)
                            .suffix(" MB"),
                    );
                    ui.label(
                        egui::RichText::new("0 表示不限")
                            .size(11.0)
                            .color(ui_colors::TEXT_3),
                    );
                });

                if !self.buf.filter().signature().is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "规则签名：{}",
                            self.buf.filter().signature()
                        ))
                        .size(10.0)
                        .monospace()
                        .color(ui_colors::TEXT_3),
                    );
                }

                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    let btn = egui::Button::new(
                        egui::RichText::new("保存").size(13.0).color(egui::Color32::WHITE),
                    )
                    .fill(ui_colors::PRIMARY)
                    .rounding(egui::Rounding::same(6.0))
                    .min_size(egui::vec2(80.0, 30.0));
                    if ui.add(btn).clicked() {
                        self.apply_buf();
                        let issues = self
                            .settings
                            .tasks
                            .get(i)
                            .map(|t| t.issues())
                            .unwrap_or_default();
                        if issues.is_empty() {
                            match self.settings.save() {
                                Ok(()) => {
                                    self.saved_hint = Some("已保存".into());
                                    self.log(i, "任务配置已保存");
                                }
                                Err(e) => self.log(i, format!("保存失败: {e}")),
                            }
                        } else {
                            self.saved_hint = Some("未保存：配置不完整".into());
                        }
                    }
                    if ui
                        .button(if paused { "恢复任务" } else { "暂停任务" })
                        .clicked()
                    {
                        do_pause = true;
                    }
                    if ui.button("删除任务").clicked() {
                        do_delete = true;
                    }
                    if let Some(h) = &self.saved_hint {
                        ui.label(
                            egui::RichText::new(h)
                                .size(11.0)
                                .color(ui_colors::TEXT_3),
                        );
                    }
                });
            });

        if !open {
            self.task_editor = None;
        }
        if do_pause {
            self.toggle_pause(i);
        }
        if do_delete {
            self.remove_task(i);
            self.task_editor = None;
        }
    }

    fn ui_raw_log(&mut self, ctx: &egui::Context) {
        let mut open = self.show_raw_log;
        egui::Window::new("运行日志")
            .open(&mut open)
            .default_width(700.0)
            .default_height(460.0)
            .show(ctx, |ui| {
                let lines = self
                    .runtimes
                    .get(self.selected)
                    .map(|r| r.logs.clone())
                    .unwrap_or_default();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if lines.is_empty() {
                            ui.label(
                                egui::RichText::new("还没有日志")
                                    .size(12.0)
                                    .color(ui_colors::TEXT_3),
                            );
                        }
                        for l in &lines {
                            ui.label(
                                egui::RichText::new(l)
                                    .size(11.0)
                                    .monospace()
                                    .color(ui_colors::TEXT_2),
                            );
                        }
                    });
            });
        self.show_raw_log = open;
    }

    /// 全窗口向导：1 同步设备 → 2 规则设置 → 3 同步路径
    fn ui_wizard(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("创建同步任务")
                    .size(16.0)
                    .strong()
                    .color(ui_colors::TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕").clicked() {
                    if let Some(w) = self.wizard.as_mut() {
                        w.stop_pairing();
                    }
                    self.wizard = None;
                }
            });
        });
        ui.add_space(12.0);

        let Some(w) = self.wizard.as_mut() else {
            return;
        };

        // 步骤条
        let steps = ["同步设备", "规则设置", "同步路径"];
        ui.horizontal(|ui| {
            for (i, label) in steps.iter().enumerate() {
                let active = i as u8 == w.step;
                let done = (i as u8) < w.step;
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                let fill = if active || done {
                    ui_colors::PRIMARY
                } else {
                    ui_colors::CARD_HOVER
                };
                ui.painter().circle_filled(rect.center(), 10.0, fill);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{}", i + 1),
                    egui::FontId::proportional(11.0),
                    egui::Color32::WHITE,
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(*label).size(13.0).color(if active {
                        ui_colors::TEXT
                    } else {
                        ui_colors::TEXT_3
                    }),
                );
                if i < steps.len() - 1 {
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new("›")
                            .size(16.0)
                            .color(ui_colors::TEXT_3),
                    );
                    ui.add_space(10.0);
                }
            }
        });
        ui.add_space(6.0);
        ui.painter().hline(
            ui.available_rect_before_wrap().x_range(),
            ui.cursor().top(),
            egui::Stroke::new(0.5_f32, ui_colors::BORDER),
        );
        ui.add_space(16.0);

        let mut finish: Option<Config> = None;
        let mut cancel = false;

        match w.step {
            0 => {
                // 两个入口，顺序与插件一致：加入在前、创建在后
                ui.horizontal(|ui| {
                    if ui.selectable_label(w.entry == 1, "加入已有同步").clicked() && w.entry != 1 {
                        w.entry = 1;
                        w.channel.clear();
                        w.workspace.clear();
                        w.token.clear();
                        w.ever_paired = false;
                        w.stop_pairing();
                    }
                    if ui.selectable_label(w.entry == 0, "创建新同步").clicked() && w.entry != 0 {
                        w.entry = 0;
                        if w.workspace.trim().is_empty() {
                            w.workspace = new_room_code();
                        }
                        if w.channel.len() != 6 {
                            w.channel = new_channel();
                        }
                        w.ever_paired = false;
                        w.stop_pairing();
                    }
                });
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("连接到中继服务器")
                        .size(14.0)
                        .strong()
                        .color(ui_colors::TEXT),
                );
                ui.add_space(10.0);
                relay_fields(ui, &mut w.host, &mut w.port);
                ui.add_space(10.0);

                // 通道码：6 位数字，念一遍就能对上；配上之后锁住，别让人改了对不上
                let locked = w.pairing_done();
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("通道码")
                            .size(12.0)
                            .color(ui_colors::TEXT_2),
                    );
                    if w.entry == 0 && !locked && ui.small_button("换一个").clicked() {
                        w.channel = new_channel();
                        w.stop_pairing();
                    }
                    if ui.small_button("复制").clicked() {
                        ui.output_mut(|o| o.copied_text = w.channel.clone());
                    }
                });
                let mut edit = egui::TextEdit::singleline(&mut w.channel)
                    .hint_text("6 位数字")
                    .desired_width(160.0)
                    .char_limit(6)
                    .font(egui::TextStyle::Monospace);
                if locked {
                    edit = edit.interactive(false);
                }
                let resp = ui.add(edit);
                if resp.changed() {
                    let digits: String = w
                        .channel
                        .chars()
                        .filter(|c| c.is_ascii_digit())
                        .take(6)
                        .collect();
                    if digits != w.channel {
                        w.channel = digits;
                    }
                }
                ui.label(
                    egui::RichText::new(if locked {
                        "配对已完成，通道码锁定；要换一组配置就点「重新配对」"
                    } else if w.entry == 0 {
                        "把这 6 位数字告诉对方，让对方填进同一个通道码"
                    } else {
                        "填对方给你的 6 位数字；房间码与密钥会自动带过来，不用手抄"
                    })
                    .size(11.0)
                    .color(ui_colors::TEXT_3),
                );

                ui.add_space(10.0);
                pair_card(ui, w);
                // 加入端配上对了、房间码却还是空的：说明对端的配置没带过来（对端可能也在「加入」）
                if w.entry == 1 && w.pairing_done() && w.workspace.trim().is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(
                            "房间码还没到：让对方用「创建新同步」在同一个通道码里再配一次，\
                             或展开下面的高级配置手动填。",
                        )
                        .size(11.0)
                        .color(ui_colors::WARN),
                    );
                }

                ui.add_space(10.0);
                egui::CollapsingHeader::new(
                    egui::RichText::new("高级：房间码 / 密钥")
                        .size(12.0)
                        .color(ui_colors::TEXT_2),
                )
                .default_open(false)
                .show(ui, |ui| {
                    room_field(ui, &mut w.workspace, &mut w.reveal_room);
                    ui.add_space(8.0);
                    field(
                        ui,
                        "密钥",
                        &mut w.token,
                        "留空则与房间码相同（中继配了房间密钥才需要填）",
                        true,
                    );
                });

                if let Some(t) = self.settings.tasks.first() {
                    ui.add_space(10.0);
                    if ui
                        .button(format!("沿用「{}」的连接", t.display_name()))
                        .clicked()
                    {
                        let (host, port) = split_server(&t.server);
                        w.host = host;
                        w.port = port;
                        w.token = t.token.clone();
                        w.workspace = t.workspace.clone();
                    }
                }
            }
            1 => {
                ui.label(
                    egui::RichText::new("同步方式与过滤规则")
                        .size(14.0)
                        .strong()
                        .color(ui_colors::TEXT),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.radio_value(&mut w.mode, SyncMode::Bidirectional, "双向同步");
                    ui.radio_value(&mut w.mode, SyncMode::UploadOnly, "仅上传");
                    ui.radio_value(&mut w.mode, SyncMode::DownloadOnly, "仅下载");
                });
                ui.label(
                    egui::RichText::new(match w.mode {
                        SyncMode::Bidirectional => "两端都改，冲突时各留一份副本",
                        SyncMode::UploadOnly => "只推本机变更；对端应设为「仅下载」",
                        SyncMode::DownloadOnly => "只收对端变更；对端应设为「仅上传」",
                    })
                    .size(11.0)
                    .color(ui_colors::TEXT_3),
                );
                ui.add_space(14.0);
                ui.checkbox(
                    &mut w.dotfiles,
                    egui::RichText::new("忽略以 . 开头的文件和目录（如 .git）").size(12.0),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("忽略扩展名")
                            .size(12.0)
                            .color(ui_colors::TEXT_2),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut w.ext)
                            .hint_text("lnk, pst, tmp")
                            .desired_width(200.0),
                    );
                    ui.label(
                        egui::RichText::new("逗号分隔，不带点")
                            .size(11.0)
                            .color(ui_colors::TEXT_3),
                    );
                });
            }
            _ => {
                ui.label(
                    egui::RichText::new("选择本地目录")
                        .size(14.0)
                        .strong()
                        .color(ui_colors::TEXT),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut w.root)
                            .hint_text("选目录，或直接粘贴路径")
                            .desired_width(330.0),
                    );
                    if ui.button("浏览…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            w.root = dir.to_string_lossy().to_string();
                        }
                    }
                });
                ui.add_space(10.0);
                field(ui, "任务名（留空则用目录名）", &mut w.name, "工作文档", false);

                ui.add_space(16.0);
                let t = w.to_task();
                let issues = t.issues();
                if issues.is_empty() {
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  ⇄  {} · 房间 {}",
                            t.root,
                            short_server(&t.server),
                            short_room(&t.workspace)
                        ))
                        .size(12.0)
                        .color(ui_colors::TEXT_2),
                    );
                } else {
                    for i in issues {
                        ui.label(
                            egui::RichText::new(format!("· {i}"))
                                .size(11.0)
                                .color(ui_colors::DANGER),
                        );
                    }
                }
            }
        }

        // 底部按钮
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if w.step == 0 {
                    // 配对没过就把原因写在按钮旁边，而不是只把按钮变灰
                    if !w.pairing_done() {
                        ui.label(
                            egui::RichText::new("先配对通过才能继续")
                                .size(11.0)
                                .color(ui_colors::TEXT_3),
                        );
                    }
                } else if ui.button("上一步").clicked() {
                    w.step -= 1;
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let last = w.step == 2;
                    let gate = w.step == 0 && !w.pairing_done();
                    let btn = egui::Button::new(
                        egui::RichText::new(if last {
                            "创建任务"
                        } else if gate {
                            "下一步（待配对）"
                        } else {
                            "下一步"
                        })
                        .size(13.0)
                        .color(egui::Color32::WHITE),
                    )
                    .fill(if gate {
                        ui_colors::PRIMARY_DIM
                    } else {
                        ui_colors::PRIMARY
                    })
                    .rounding(egui::Rounding::same(6.0))
                    .min_size(egui::vec2(104.0, 32.0));
                    if ui.add_enabled(!gate, btn).clicked() {
                        if last {
                            let task = w.to_task();
                            // 目录不存在就先建出来（与插件一致：新建任务时自动创建），
                            // 建不出来再由 issues() 拦住并说明。手打路径才不会白填一遍。
                            if !task.root.is_empty()
                                && !std::path::Path::new(&task.root).is_dir()
                            {
                                let _ = std::fs::create_dir_all(&task.root);
                            }
                            let issues = task.issues();
                            if issues.is_empty() {
                                finish = Some(task);
                            } else {
                                // 以前这里是「什么都不发生」，用户只能猜。
                                // 现在把还差哪几项直接写在按钮旁边。
                                w.note = format!("还差 {} 项：{}", issues.len(), issues.join("；"));
                            }
                        } else {
                            w.step += 1;
                        }
                    }
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                    if !w.note.is_empty() {
                        ui.label(
                            egui::RichText::new(&w.note)
                                .size(11.0)
                                .color(ui_colors::WARN),
                        );
                    }
                });
            });
        });

        if let Some(t) = finish {
            self.add_task(t);
            if let Some(w) = self.wizard.as_mut() {
                w.stop_pairing();
            }
            self.wizard = None;
        } else if cancel {
            if let Some(w) = self.wizard.as_mut() {
                w.stop_pairing();
            }
            self.wizard = None;
        }
    }
}

/// 配对卡片：文案与插件同一套（状态 + 一句原因 + 配对房间名 + 重新配对）。
fn pair_card(ui: &mut egui::Ui, w: &mut Wizard) {
    let phase = w.pair_phase;
    let sticky = w.ever_paired && phase != PairPhase::Denied;
    let (mark, tone) = match phase {
        PairPhase::Paired => ("✓", ui_colors::SUCCESS),
        PairPhase::Waiting | PairPhase::Connecting if sticky => ("✓", ui_colors::SUCCESS),
        PairPhase::Waiting | PairPhase::Connecting => ("⏳", ui_colors::WARN),
        PairPhase::Denied | PairPhase::Unreachable => ("✗", ui_colors::DANGER),
        PairPhase::Idle => ("·", ui_colors::TEXT_3),
    };

    egui::Frame::none()
        .fill(ui_colors::CARD)
        .stroke(egui::Stroke::new(0.5_f32, ui_colors::BORDER))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::same(12.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("{mark} {}", phase.title(w.ever_paired)))
                        .size(13.0)
                        .strong()
                        .color(tone),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("重新配对").clicked() {
                        w.ever_paired = false;
                        w.pair_phase = PairPhase::Idle;
                        w.stop_pairing();
                    }
                });
            });
            ui.label(
                egui::RichText::new(pair_hint(w))
                    .size(11.0)
                    .color(ui_colors::TEXT_3),
            );
            if !w.channel.trim().is_empty() {
                ui.label(
                    egui::RichText::new(format!("配对房间 {}::pair", w.channel.trim()))
                        .size(11.0)
                        .color(ui_colors::TEXT_3)
                        .monospace(),
                );
            }
        });
}

fn pair_hint(w: &Wizard) -> String {
    match w.pair_phase {
        PairPhase::Paired => {
            "两端已经在中继里互相看见，可以继续；任务建好后会自动开始同步。".into()
        }
        PairPhase::Waiting if w.ever_paired => {
            "已经配过对，门禁不再拦；对端这会儿不在配对房间是正常的。".into()
        }
        PairPhase::Waiting => {
            "这个配对房间现在只有你一端。让对方在通道码里填同一个 6 位数字。".into()
        }
        PairPhase::Connecting => "后台正在连中继，一两秒后会有结果。".into(),
        PairPhase::Denied => {
            let said = w.pair_error.trim();
            if said.is_empty() {
                "中继拒绝了配对通道。".into()
            } else {
                format!("中继回的是「{said}」。")
            }
        }
        PairPhase::Unreachable => {
            let said = w.pair_error.trim();
            if said.is_empty() {
                "连不上中继：检查地址与端口。".into()
            } else {
                said.to_string()
            }
        }
        PairPhase::Idle => "填好中继地址与 6 位通道码会自动开始配对。".into(),
    }
}

fn nav_item(ui: &mut egui::Ui, selected: bool, label: &str) -> bool {
    let w = ui.available_width();
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(w, 34.0), egui::Sense::click());
    let hovered = resp.hovered();
    if selected {
        ui.painter().rect_filled(rect, 6.0, ui_colors::PRIMARY);
    } else if hovered {
        ui.painter().rect_filled(rect, 6.0, ui_colors::CARD_HOVER);
    }
    let color = if selected {
        egui::Color32::WHITE
    } else {
        ui_colors::TEXT_2
    };
    ui.painter().text(
        rect.left_center() + egui::vec2(12.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        color,
    );
    resp.clicked()
}

/// 文件夹图标 + 右下角绿色对勾（对应飞牛任务卡左侧图标）
fn draw_folder(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(34.0, 30.0), egui::Sense::hover());
    let p = ui.painter();
    let body = egui::Rect::from_min_size(
        rect.min + egui::vec2(0.0, 5.0),
        egui::vec2(30.0, 22.0),
    );
    p.rect_filled(body, 4.0, ui_colors::PRIMARY);
    let tab = egui::Rect::from_min_size(
        rect.min + egui::vec2(0.0, 5.0),
        egui::vec2(14.0, 5.0),
    );
    p.rect_filled(tab, 2.0, ui_colors::PRIMARY);
    // 对勾
    let c = egui::pos2(rect.max.x - 4.0, rect.max.y - 4.0);
    p.circle_filled(c, 7.0, ui_colors::SUCCESS);
    p.line_segment(
        [c + egui::vec2(-3.5, 0.0), c + egui::vec2(-1.0, 2.6)],
        egui::Stroke::new(1.6_f32, egui::Color32::WHITE),
    );
    p.line_segment(
        [c + egui::vec2(-1.0, 2.6), c + egui::vec2(3.5, -2.4)],
        egui::Stroke::new(1.6_f32, egui::Color32::WHITE),
    );
}

/// 圆形暂停/继续按钮
fn pause_button(ui: &mut egui::Ui, paused: bool) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::click());
    let p = ui.painter();
    let color = if resp.hovered() {
        ui_colors::TEXT
    } else {
        ui_colors::TEXT_2
    };
    p.circle_stroke(rect.center(), 12.0, egui::Stroke::new(1.0_f32, color));
    if paused {
        // 播放三角
        p.add(egui::Shape::convex_polygon(
            vec![
                rect.center() + egui::vec2(-3.0, -5.0),
                rect.center() + egui::vec2(5.0, 0.0),
                rect.center() + egui::vec2(-3.0, 5.0),
            ],
            color,
            egui::Stroke::NONE,
        ));
    } else {
        // 暂停双竖条
        p.rect_filled(
            egui::Rect::from_center_size(
                rect.center() + egui::vec2(-2.5, 0.0),
                egui::vec2(2.0, 9.0),
            ),
            1.0,
            color,
        );
        p.rect_filled(
            egui::Rect::from_center_size(
                rect.center() + egui::vec2(2.5, 0.0),
                egui::vec2(2.0, 9.0),
            ),
            1.0,
            color,
        );
    }
    resp.clicked()
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(18.0);
    ui.label(
        egui::RichText::new(title)
            .size(13.0)
            .strong()
            .color(ui_colors::TEXT_2),
    );
    ui.add_space(6.0);
}

/// `ws://10.90.45.64:10023/ws` → ("10.90.45.64", "10023")；没写端口就留空（交给 8443 兜底）。
fn split_server(server: &str) -> (String, String) {
    let raw = server.trim();
    let rest = raw
        .strip_prefix("ws://")
        .or_else(|| raw.strip_prefix("wss://"))
        .unwrap_or(raw);
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return (String::new(), String::new());
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (host.to_string(), port.to_string())
        }
        _ => (authority.to_string(), String::new()),
    }
}

/// 两栏拼回 daemon 认的整条地址；端口留空按 8443（daemon 的默认端口）。
fn compose_server(host: &str, port: &str) -> String {
    let host = host.trim();
    if host.is_empty() {
        return String::new();
    }
    let digits: String = port
        .trim()
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(5)
        .collect();
    if digits.is_empty() {
        format!("ws://{host}:8443/ws")
    } else {
        format!("ws://{host}:{digits}/ws")
    }
}

/// 卡片上只露前 8 位：整串 33 位会把一行顶爆，完整值在设置里看。
fn short_room(room: &str) -> String {
    let chars: Vec<char> = room.chars().collect();
    if chars.len() > 12 {
        format!("{}…", chars[..8].iter().collect::<String>())
    } else {
        room.to_string()
    }
}

/// 房间码：`k` + 32 位十六进制，和 dsh 插件生成的同构——32 位随机、猜不到就是门禁。
fn new_room_code() -> String {
    format!("k{}", uuid::Uuid::new_v4().simple())
}

/// 6 位通道码：只负责在配对房间里「见面」，念一遍就能对上（与插件同一规则）。
fn new_channel() -> String {
    let n = uuid::Uuid::new_v4().as_u128() % 1_000_000;
    format!("{n:06}")
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str, password: bool) {
    ui.label(
        egui::RichText::new(label)
            .size(12.0)
            .color(ui_colors::TEXT_2),
    );
    let mut edit = egui::TextEdit::singleline(value)
        .hint_text(hint)
        .desired_width(420.0);
    if password {
        edit = edit.password(true);
    }
    ui.add(edit);
}

/// 中继地址两栏：IP 与端口分开填，省得手打 `ws://…/ws`（与 dsh 插件一致）。
fn relay_fields(ui: &mut egui::Ui, host: &mut String, port: &mut String) {
    ui.label(egui::RichText::new("中继地址").size(12.0).color(ui_colors::TEXT_2));
    ui.horizontal(|ui| {
        let host_resp = ui.add(
            egui::TextEdit::singleline(host)
                .hint_text("10.90.45.64")
                .desired_width(250.0),
        );
        // 整条地址粘进 IP 栏也认：带冒号或斜杠就自动拆到两栏
        if host_resp.changed() && (host.contains(':') || host.contains('/')) {
            let (h, p) = split_server(host);
            *host = h;
            if !p.is_empty() {
                *port = p;
            }
        }
        let resp = ui.add(
            egui::TextEdit::singleline(port)
                .hint_text("端口 8443")
                .desired_width(110.0),
        );
        // 端口只收数字：粘贴整条地址时别把 `ws://` 也塞进来
        if resp.changed() {
            let digits: String = port
                .chars()
                .filter(|c| c.is_ascii_digit())
                .take(5)
                .collect();
            if digits != *port {
                *port = digits;
            }
        }
    });
    ui.label(
        egui::RichText::new("端口填 docker -p 左边那个（-p 10023:8443 就是 10023）；留空按 8443")
            .size(11.0)
            .color(ui_colors::TEXT_3),
    );
}

/// 房间码：自动生成、默认打码，和插件的「高级配置」同一个口径。
fn room_field(ui: &mut egui::Ui, value: &mut String, reveal: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("房间码（两端必须相同）")
                .size(12.0)
                .color(ui_colors::TEXT_2),
        );
        if ui.small_button(if *reveal { "隐藏" } else { "显示" }).clicked() {
            *reveal = !*reveal;
        }
        if ui.small_button("换一个").clicked() {
            *value = new_room_code();
        }
    });
    let mut edit = egui::TextEdit::singleline(value)
        .hint_text("自动生成")
        .desired_width(420.0);
    if !*reveal {
        edit = edit.password(true);
    }
    ui.add(edit);
    ui.label(
        egui::RichText::new("32 位随机码，默认隐藏；两台填同一个房间码才是同一间房")
            .size(11.0)
            .color(ui_colors::TEXT_3),
    );
}

fn mode_short(m: SyncMode) -> &'static str {
    match m {
        SyncMode::Bidirectional => "双向",
        SyncMode::UploadOnly => "仅上传",
        SyncMode::DownloadOnly => "仅下载",
    }
}

fn filter_summary(f: &Filter) -> String {
    let mut parts: Vec<String> = Vec::new();
    if f.ignore_dotfiles {
        parts.push("忽略点文件".into());
    }
    if !f.ignore_extensions.is_empty() {
        parts.push(format!("忽略 {}", f.ignore_extensions.join("/")));
    }
    if f.size_min.is_some() || f.size_max.is_some() {
        parts.push("按大小过滤".into());
    }
    if parts.is_empty() {
        "无过滤".into()
    } else {
        parts.join(" · ")
    }
}

fn short_server(s: &str) -> String {
    s.trim()
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .trim_end_matches("/ws")
        .to_string()
}

fn computer_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "本机".into())
}

fn jitter() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        % 7
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_stamp() -> String {
    fmt_ts(now_secs())
}

fn fmt_ts(secs: i64) -> String {
    let local = secs + 8 * 3600;
    let days = local.div_euclid(86400);
    let rem = local.rem_euclid(86400);
    let (_, m, d) = civil(days);
    format!("{m:02}-{d:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

fn fmt_ts_full(secs: i64) -> String {
    let local = secs + 8 * 3600;
    let days = local.div_euclid(86400);
    let rem = local.rem_euclid(86400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn stamp_full() -> String {
    let secs = now_secs();
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil(z: i64) -> (i32, u32, u32) {
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

#[cfg(test)]
mod pair_tests {
    use super::*;

    /// 创建端把「房间码|密钥」挂在 device id 上，加入端要能原样解出来。
    #[test]
    fn announce_roundtrip() {
        let room = "k0123456789abcdef0123456789abcdef";
        let payload = format!("{room}|secret");
        let device = format!("gui-abc~{}", hex_of(&payload));
        assert_eq!(
            parse_announce(&device),
            Some((room.to_string(), "secret".to_string()))
        );
        // 加入端没有载荷、或者半截的十六进制，都不该解出东西
        assert_eq!(parse_announce("gui-abc"), None);
        assert_eq!(parse_announce("gui-abc~zz"), None);
    }

    #[test]
    fn server_split_compose() {
        assert_eq!(
            split_server("ws://10.90.45.64:10023/ws"),
            ("10.90.45.64".to_string(), "10023".to_string())
        );
        assert_eq!(
            split_server("wss://sync.example.com/ws"),
            ("sync.example.com".to_string(), String::new())
        );
        assert_eq!(compose_server("10.90.45.64", "10023"), "ws://10.90.45.64:10023/ws");
        assert_eq!(compose_server("10.90.45.64", ""), "ws://10.90.45.64:8443/ws");
        assert_eq!(compose_server("", "10023"), "");
    }

    #[test]
    fn channel_is_six_digits() {
        for _ in 0..64 {
            let c = new_channel();
            assert_eq!(c.len(), 6, "{c}");
            assert!(c.chars().all(|ch| ch.is_ascii_digit()), "{c}");
        }
    }

    /// 创建端的密钥留空 = 跟随房间码；否则 Config::issues() 会一直拦着「创建任务」。
    #[test]
    fn creator_token_follows_room_code() {
        let mut w = Wizard::new(None);
        w.host = "10.90.45.64".into();
        w.port = "10023".into();
        w.root = std::env::temp_dir().to_string_lossy().to_string();
        w.token = String::new();
        let t = w.to_task();
        assert_eq!(t.token, t.workspace, "密钥留空应当跟随房间码");
        assert!(
            t.issues().is_empty(),
            "地址与目录都填好之后不该再有 issue：{:?}",
            t.issues()
        );
    }

    #[test]
    fn room_code_shape() {
        let code = new_room_code();
        assert!(code.starts_with('k'));
        assert_eq!(code.len(), 33);
        assert!(code[1..].chars().all(|c| c.is_ascii_hexdigit()));
    }
}
