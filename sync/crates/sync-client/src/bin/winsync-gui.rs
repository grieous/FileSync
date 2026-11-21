//! WinSync 图形界面。
//!
//! 布局参照同类成熟产品（飞牛同步）的组织方式：
//!   同步 —— 连接设置、同步模式、过滤规则、状态与进度
//!   记录 —— 文件级操作记录（谁、什么动作、成功还是失败）
//!   日志 —— 原始运行日志，排查用
//!
//! 同步在后台线程跑（独立 tokio runtime），界面通过 channel 收事件，不会被阻塞。
//! release 编译时用 `windows_subsystem = "windows"` 隐藏黑色控制台窗口。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;
use notify::Watcher;
use sync_client::config::SyncMode;
use sync_client::session::{self, Event, Report};
use sync_client::Config;
use sync_core::{Activity, Filter, Store};

enum WorkerMsg {
    Event(Event),
    Done(Result<Report, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Records,
    Logs,
    Settings,
}

/// 配色取自飞牛同步 Web 端的 Semi Design 变量（`--semi-brand-5` 等）。
mod semi {
    use eframe::egui::Color32;

    pub const PRIMARY: Color32 = Color32::from_rgb(0x21, 0x73, 0xDF);
    pub const TEXT: Color32 = Color32::from_rgb(0x1C, 0x1F, 0x23);
    pub const TEXT_2: Color32 = Color32::from_rgb(0x6B, 0x72, 0x80);
    pub const TEXT_3: Color32 = Color32::from_rgb(0x9A, 0xA0, 0xA8);
    pub const BORDER: Color32 = Color32::from_rgb(0xE5, 0xE7, 0xEB);
    pub const BG: Color32 = Color32::from_rgb(0xF7, 0xF8, 0xFA);
    pub const SUCCESS: Color32 = Color32::from_rgb(0x00, 0xB4, 0x2A);
    pub const DANGER: Color32 = Color32::from_rgb(0xF5, 0x3F, 0x3F);
    pub const WARN: Color32 = Color32::from_rgb(0xFF, 0x9A, 0x00);
    pub const WHITE: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
}

/// 浅色主题 + 品牌色。默认的 egui 深色主题与 NAS 类工具的气质不搭。
fn setup_style(ctx: &egui::Context) {
    let mut v = egui::Visuals::light();
    v.panel_fill = semi::BG;
    v.window_fill = semi::WHITE;
    v.extreme_bg_color = semi::WHITE;
    v.override_text_color = Some(semi::TEXT);
    v.hyperlink_color = semi::PRIMARY;
    v.selection.bg_fill = semi::PRIMARY.gamma_multiply(0.28);
    v.widgets.noninteractive.bg_fill = semi::WHITE;
    v.widgets.inactive.bg_fill = semi::WHITE;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, semi::BORDER);
    v.widgets.hovered.bg_fill = egui::Color32::from_rgb(0xF2, 0xF3, 0xF5);
    v.widgets.active.bg_fill = egui::Color32::from_rgb(0xE8, 0xEA, 0xED);
    ctx.set_visuals(v);
}

fn main() -> eframe::Result<()> {
    // 单实例：同一台机器只允许一个进程。
    // 两个实例会共用同一份 state.db，拿到相同 device_id，
    // 配对后会互相死锁——这个坑之前已经踩过一次。
    #[cfg(windows)]
    if !acquire_single_instance() {
        rfd::MessageDialog::new()
            .set_title("WinSync")
            .set_description("WinSync 已经在运行了。\n请在任务栏里找到已有的窗口，不要重复启动。")
            .set_level(rfd::MessageLevel::Info)
            .show();
        return Ok(());
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([640.0, 760.0])
        .with_min_inner_size([560.0, 600.0])
        .with_title(format!("WinSync v{} — 文件夹同步", env!("CARGO_PKG_VERSION")));

    if let Some((w, h, rgba)) = app_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(egui::IconData {
            rgba,
            width: w,
            height: h,
        }));
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "WinSync",
        options,
        Box::new(|cc| {
            setup_fonts(&cc.egui_ctx);
            setup_style(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
}

/// 命名互斥体实现单实例。
/// 句柄故意不关闭——进程退出时系统自动回收，这样"是否已存在"的判断才可靠。
#[cfg(windows)]
fn acquire_single_instance() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    unsafe {
        let name: Vec<u16> = "Local\\WinSync_SingleInstance_v1\0"
            .encode_utf16()
            .collect();
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if (handle as isize) == 0 {
            // 创建失败就不拦着，宁可多开也不能打不开
            return true;
        }
        GetLastError() != ERROR_ALREADY_EXISTS
    }
}

/// egui 内置字体没有中文，必须挂一个系统中文字体，否则全是方块。
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // 优先用纯 ttf（.ttc 字体集合 ab_glyph 未必能解析）
    let candidates = [
        r"C:\Windows\Fonts\simhei.ttf", // 黑体
        r"C:\Windows\Fonts\deng.ttf",   // 等线
        r"C:\Windows\Fonts\msyh.ttc",   // 微软雅黑
        r"C:\Windows\Fonts\simsun.ttc", // 宋体
    ];

    for path in candidates {
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

/// 从打包进二进制的 .ico 里取出 64×64 那档，转成 RGBA。
/// 窗口图标与托盘图标共用这份数据，和 exe 资源段里的图标完全一致。
fn app_icon() -> Option<(u32, u32, Vec<u8>)> {
    let bytes: &[u8] = include_bytes!("../../winres/app.ico");
    if bytes.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut best: Option<(usize, usize, usize)> = None; // (边长, 偏移, 长度)
    for i in 0..count {
        let e = 6 + i * 16;
        if e + 16 > bytes.len() {
            break;
        }
        let w = if bytes[e] == 0 {
            256
        } else {
            bytes[e] as usize
        };
        let len = u32::from_le_bytes(bytes[e + 8..e + 12].try_into().ok()?) as usize;
        let off = u32::from_le_bytes(bytes[e + 12..e + 16].try_into().ok()?) as usize;
        // 优先正好 64，否则取不超过 64 的最大一档
        let better = match best {
            None => true,
            Some((bw, _, _)) => {
                (w == 64 && bw != 64) || (w <= 64 && w > bw && bw != 64) || (w > 64 && bw > 64 && w < bw)
            }
        };
        if better && off + len <= bytes.len() {
            best = Some((w, off, len));
        }
    }
    let (w, off, _) = best?;

    // BITMAPINFOHEADER：宽度、高度（含掩码，是实际高度的 2 倍）、位深
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
            rgba[d] = bytes[s + 2]; // R
            rgba[d + 1] = bytes[s + 1]; // G
            rgba[d + 2] = bytes[s]; // B
            rgba[d + 3] = bytes[s + 3]; // A
        }
    }
    Some((w as u32, h as u32, rgba))
}

#[cfg(windows)]
struct Tray {
    _icon: tray_icon::TrayIcon,
    open: tray_icon::menu::MenuItem,
    sync: tray_icon::menu::MenuItem,
    auto: tray_icon::menu::MenuItem,
    quit: tray_icon::menu::MenuItem,
}

#[cfg(windows)]
fn build_tray() -> Option<Tray> {
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};

    let open = MenuItem::with_id("open", "打开主界面", true, None);
    let sync = MenuItem::with_id("sync", "立即同步", true, None);
    let auto = MenuItem::with_id("auto", "暂停自动同步", true, None);
    let quit = MenuItem::with_id("quit", "退出", true, None);

    let menu = Menu::new();
    let _ = menu.append(&open);
    let _ = menu.append(&sync);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&auto);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit);

    let (w, h, rgba) = app_icon()?;
    let icon = tray_icon::Icon::from_rgba(rgba, w, h).ok()?;
    let tray = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("WinSync — 文件夹同步")
        .with_icon(icon)
        .build()
        .ok()?;

    Some(Tray {
        _icon: tray,
        open,
        sync,
        auto,
        quit,
    })
}

struct App {
    cfg: Config,
    server: String,
    token: String,
    workspace: String,
    root: String,
    mode: SyncMode,

    // 过滤规则在界面上用更友好的单位编辑，保存时再折算成字节
    filter_dotfiles: bool,
    filter_ext: String,
    filter_min_mb: f64,
    filter_max_mb: f64,

    tab: Tab,
    records: Vec<Activity>,
    records_loaded: bool,

    running: bool,
    auto: bool,
    next_auto: Instant,

    logs: Vec<String>,
    progress: Option<(u64, u64, String)>,
    last: Option<Result<Report, String>>,
    last_time: Option<String>,
    saved_hint: Option<String>,
    /// 上一次打印过的配置问题，避免每帧刷同一句话
    last_issue_log: String,

    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,

    /// 目录监听器。必须始终持有，drop 掉监听就失效了。
    watcher: Option<notify::RecommendedWatcher>,
    watch_tx: Sender<()>,
    watch_rx: Receiver<()>,
    /// 当前已挂上监听的目录，用于判断何时需要重建
    watch_root: String,
    /// 收到变更但还没同步
    dirty: bool,
    /// 最后一次变更时间，用于去抖
    last_change: Instant,

    /// 托盘。创建失败时为 None，程序退化为普通窗口程序（不能因为托盘挂了就打不开）。
    #[cfg(windows)]
    tray: Option<Tray>,
    /// 走托盘菜单退出：此时不再拦截关窗事件
    quitting: bool,
    /// 「已隐藏到托盘」只提示一次
    hidden_notice: bool,
}

impl App {
    fn new() -> Self {
        // 以前这里用 .ok().flatten() 把解析错误吞掉了，配置读不出来时表现得
        // 跟"没配置"完全一样，用户无从判断。现在把错误带出来写进日志。
        let (cfg, load_err) = match Config::load() {
            Ok(Some(c)) => (c, None),
            Ok(None) => (Config::default(), None),
            Err(e) => (
                Config::default(),
                Some(format!(
                    "读取配置失败：{e}\n已退回默认值，请检查 {}",
                    Config::file().display()
                )),
            ),
        };

        let (tx, rx) = channel();
        let (watch_tx, watch_rx) = channel();
        let mode = cfg.mode;
        let filter_dotfiles = cfg.filter.ignore_dotfiles;
        let filter_ext = cfg.filter.ignore_extensions.join(",");
        let filter_min_mb = cfg.filter.size_min.map(|b| b as f64 / MB).unwrap_or(0.0);
        let filter_max_mb = cfg.filter.size_max.map(|b| b as f64 / MB).unwrap_or(0.0);

        let mut app = Self {
            server: cfg.server.clone(),
            token: cfg.token.clone(),
            workspace: cfg.workspace.clone(),
            root: cfg.root.clone(),
            mode,
            filter_dotfiles,
            filter_ext,
            filter_min_mb,
            filter_max_mb,
            cfg,
            tab: Tab::Overview,
            records: Vec::new(),
            records_loaded: false,
            running: false,
            auto: true,
            next_auto: Instant::now() + Duration::from_secs(5),
            logs: Vec::new(),
            progress: None,
            last: None,
            last_time: None,
            saved_hint: None,
            last_issue_log: String::new(),
            tx,
            rx,
            watcher: None,
            watch_tx,
            watch_rx,
            watch_root: String::new(),
            dirty: false,
            last_change: Instant::now(),
            #[cfg(windows)]
            tray: build_tray(),
            quitting: false,
            hidden_notice: false,
        };

        #[cfg(windows)]
        if app.tray.is_some() {
            app.log("托盘已就绪：关闭窗口不会退出，同步继续运行");
        }

        if let Some(e) = load_err {
            app.log(e);
        }
        app
    }

    /// 逐项检查配置，返回人能看懂的问题列表。
    /// 之前这里只判断 is_ready() 就静默跳过，用户完全不知道为什么不工作。
    fn issues(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();

        let server = self.server.trim();
        if server.is_empty() {
            v.push("未填写服务器地址".into());
        } else if !server.starts_with("ws://") && !server.starts_with("wss://") {
            v.push("服务器地址必须以 ws:// 或 wss:// 开头".into());
        } else if !server.contains("/ws") {
            v.push("服务器地址通常以 /ws 结尾，例如 ws://192.168.3.222:18443/ws".into());
        }

        if self.token.trim().is_empty() {
            v.push("未填写密钥（要与服务器的 SYNC_TOKEN 完全一致）".into());
        }
        if self.workspace.trim().is_empty() {
            v.push("未填写房间名（两台设备必须相同）".into());
        }

        let root = self.root.trim();
        if root.is_empty() {
            v.push("未选择同步目录".into());
        } else if !std::path::Path::new(root).is_dir() {
            v.push(format!("同步目录不存在：{root}"));
        }

        if self.filter_min_mb > 0.0
            && self.filter_max_mb > 0.0
            && self.filter_min_mb >= self.filter_max_mb
        {
            v.push("过滤规则里「最小大小」不应大于等于「最大大小」".into());
        }

        v
    }

    fn filter(&self) -> Filter {
        Filter {
            ignore_dotfiles: self.filter_dotfiles,
            ignore_extensions: self
                .filter_ext
                .split(',')
                .map(|s| s.trim().trim_start_matches('.').to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            size_min: if self.filter_min_mb > 0.0 {
                Some((self.filter_min_mb * MB) as u64)
            } else {
                None
            },
            size_max: if self.filter_max_mb > 0.0 {
                Some((self.filter_max_mb * MB) as u64)
            } else {
                None
            },
        }
    }

    fn current(&self) -> Config {
        Config {
            server: self.server.trim().to_string(),
            token: self.token.trim().to_string(),
            workspace: self.workspace.trim().to_string(),
            root: self.root.trim().to_string(),
            mode: self.mode,
            filter: self.filter(),
        }
    }

    fn log(&mut self, s: impl Into<String>) {
        let s = s.into();
        // 同时落盘：出问题时用户可以直接把日志文件发过来，不用靠回忆界面内容
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
        self.logs.push(s);
        if self.logs.len() > 200 {
            self.logs.remove(0);
        }
    }

    fn reload_records(&mut self) {
        self.records.clear();
        match Store::open(&Config::state_db()).and_then(|s| s.recent_activities(300)) {
            Ok(list) => self.records = list,
            Err(e) => self.log(format!("读取记录失败: {e}")),
        }
        self.records_loaded = true;
    }

    fn start_sync(&mut self, wait_peer_secs: u64) {
        if self.running {
            return;
        }
        let cfg = self.current();
        let issues = self.issues();
        if !issues.is_empty() {
            self.log(format!("无法同步：{}", issues.join("；")));
            return;
        }

        // 每次同步前自动落盘。
        // 之前界面里改了路径但没点"保存配置"，程序重启后读回旧路径，
        // 两台机器指向不同目录，互相误判对方删了文件——这是上次事故的直接原因。
        if let Err(e) = cfg.save() {
            self.log(format!("配置自动保存失败: {e}"));
        }

        self.running = true;
        self.progress = None;
        self.log(format!("开始同步 {}", cfg.root));

        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(WorkerMsg::Done(Err(format!("运行时创建失败: {e}"))));
                    return;
                }
            };

            let (evt_tx, evt_rx) = channel::<Event>();
            let tx_fwd = tx.clone();
            let fwd = std::thread::spawn(move || {
                while let Ok(e) = evt_rx.recv() {
                    if tx_fwd.send(WorkerMsg::Event(e)).is_err() {
                        break;
                    }
                }
            });

            let res = rt.block_on(session::run(&cfg, Some(evt_tx), true, true, wait_peer_secs));
            let _ = fwd.join();
            let _ = tx.send(WorkerMsg::Done(res.map_err(|e| format!("{e:#}"))));
        });
    }
}

const MB: f64 = 1024.0 * 1024.0;

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ---- 托盘菜单 / 双击图标 ----
        #[cfg(windows)]
        {
            while let Ok(ev) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                match ev.id.0.as_str() {
                    "open" => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    "sync" => self.start_sync(120),
                    "auto" => {
                        self.auto = !self.auto;
                        let text = if self.auto {
                            "暂停自动同步"
                        } else {
                            "恢复自动同步"
                        };
                        if let Some(t) = &self.tray {
                            let _ = t.auto.set_text(text);
                        }
                        self.log(if self.auto {
                            "已恢复自动同步"
                        } else {
                            "已暂停自动同步"
                        });
                    }
                    "quit" => {
                        self.quitting = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    _ => {}
                }
            }
            while let Ok(ev) = tray_icon::TrayIconEvent::receiver().try_recv() {
                if matches!(ev, tray_icon::TrayIconEvent::DoubleClick { .. }) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
            }
        }

        // 关窗口 = 隐藏到托盘（同步继续）。真正退出走托盘菜单。
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            #[cfg(windows)]
            if self.tray.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                if !self.hidden_notice {
                    self.log("已隐藏到托盘，同步继续运行（退出请用托盘菜单）");
                    self.hidden_notice = true;
                }
            }
        }

        // 收后台消息
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Event(Event::Log(s)) => {
                    self.logs.push(s);
                    if self.logs.len() > 200 {
                        self.logs.remove(0);
                    }
                }
                WorkerMsg::Event(Event::Progress { done, total, what }) => {
                    self.progress = Some((done, total, what));
                }
                WorkerMsg::Done(res) => {
                    self.running = false;
                    self.progress = None;
                    self.last_time = Some(now_stamp());
                    // 同步过程自己会写文件，产生的监听事件必须丢掉，
                    // 否则会立刻又判定"有变更"，形成自激循环。
                    self.dirty = false;
                    while self.watch_rx.try_recv().is_ok() {}
                    match &res {
                        Ok(r) if r.up_to_date => self.log("两端已一致，无需传输"),
                        Ok(r) => self.log(format!(
                            "完成：推送 {}，拉取 {}，删除 {}，冲突 {}",
                            r.pushed, r.pulled, r.deleted, r.conflicts
                        )),
                        Err(e) => self.log(format!("失败：{e}")),
                    }
                    self.last = Some(res);
                    if self.records_loaded {
                        self.reload_records();
                    }
                }
            }
        }

        // ---- 目录监听 ----
        // 换目录后需要重建 watcher；root 为空或不存在时挂不上，交给轮询兜底。
        if self.watch_root != self.root {
            self.watcher = None;
            self.watch_root = String::new();
            let path = std::path::PathBuf::from(self.root.trim());
            if path.is_dir() {
                let tx = self.watch_tx.clone();
                if let Ok(mut w) = notify::recommended_watcher(
                    move |res: notify::Result<notify::Event>| {
                        if let Ok(ev) = res {
                            // 只关心内容变更，忽略纯访问事件，否则读文件也会触发同步
                            if !matches!(ev.kind, notify::EventKind::Access(_)) {
                                let _ = tx.send(());
                            }
                        }
                    },
                ) {
                    if w.watch(&path, notify::RecursiveMode::Recursive).is_ok() {
                        self.watcher = Some(w);
                        self.watch_root = self.root.clone();
                        self.log(format!("已开始监听目录：{}", path.display()));
                    }
                }
            }
        }

        let mut got_change = false;
        while self.watch_rx.try_recv().is_ok() {
            got_change = true;
        }
        if got_change {
            self.dirty = true;
            self.last_change = Instant::now();
        }

        // ---- 触发同步 ----
        // 两个来源：目录有变更（去抖 3 秒），或兜底轮询。
        // 轮询不能去掉——notify 在休眠唤醒、缓冲区溢出、网络盘场景下会丢事件。
        let due_poll = Instant::now() >= self.next_auto;
        let due_change = self.dirty && self.last_change.elapsed() > Duration::from_secs(3);

        if self.auto && !self.running && (due_poll || due_change) {
            let issues = self.issues();
            if issues.is_empty() {
                // 关键：等待时间必须接近轮询周期。
                // 之前等 8 秒、周期 30 秒，两端相位差十几秒就永远碰不上——
                // 各自只在线 8s/30s，重叠概率不到 30%，且固定相位会长期错开。
                self.start_sync(25);
            } else {
                let msg = format!("自动同步已跳过：{}", issues.join("；"));
                if self.last_issue_log != msg {
                    self.log(msg.clone());
                    self.last_issue_log = msg;
                }
            }
            // 加随机抖动，避免两台设备周期共振、长期错开
            let jitter = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(0)
                % 7;
            self.next_auto = Instant::now() + Duration::from_secs(28 + jitter);
            self.dirty = false;
        }

        // 记录页按需加载，切过去时刷新一次
        if self.tab == Tab::Records && !self.records_loaded {
            self.reload_records();
        }

        egui::TopBottomPanel::top("head")
            .frame(
                egui::Frame::none()
                    .fill(semi::WHITE)
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (color, text) = if self.running {
                        (semi::WARN, "同步中")
                    } else {
                        match &self.last {
                            Some(Ok(_)) => (semi::SUCCESS, "正常"),
                            Some(Err(_)) => (semi::DANGER, "有错误"),
                            None => (semi::TEXT_3, "尚未同步"),
                        }
                    };
                    ui.label(
                        egui::RichText::new("WinSync")
                            .size(17.0)
                            .strong()
                            .color(semi::TEXT),
                    );
                    ui.label(
                        egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                            .size(11.0)
                            .color(semi::TEXT_3),
                    );
                    ui.add_space(10.0);
                    ui.label(egui::RichText::new("●").size(11.0).color(color));
                    ui.label(egui::RichText::new(text).size(12.0).color(color));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(t) = &self.last_time {
                            ui.label(
                                egui::RichText::new(format!("上次同步 {t}"))
                                    .size(12.0)
                                    .color(semi::TEXT_3),
                            );
                        }
                    });
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    for (tab, label) in [
                        (Tab::Overview, "概览"),
                        (Tab::Records, "记录"),
                        (Tab::Logs, "日志"),
                        (Tab::Settings, "设置"),
                    ] {
                        let selected = self.tab == tab;
                        let text = egui::RichText::new(label).size(13.0).color(if selected {
                            semi::PRIMARY
                        } else {
                            semi::TEXT_2
                        });
                        if ui.selectable_label(selected, text).clicked() {
                            self.tab = tab;
                        }
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Overview => self.ui_overview(ui),
            Tab::Records => self.ui_records(ui),
            Tab::Logs => self.ui_logs(ui),
            Tab::Settings => self.ui_settings(ui),
        });

        if self.running || self.auto {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

impl App {
    /// 概览：任务卡 + 状态 + 主操作。对齐飞牛同步的首屏结构。
    fn ui_overview(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);

        let card = egui::Frame::none()
            .fill(semi::WHITE)
            .stroke(egui::Stroke::new(1.0, semi::BORDER))
            .rounding(egui::Rounding::same(12.0))
            .inner_margin(egui::Margin::same(16.0));

        card.show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(if self.root.trim().is_empty() {
                        "未选择同步目录"
                    } else {
                        self.root.trim()
                    })
                    .size(15.0)
                    .strong()
                    .color(semi::TEXT),
                );
                ui.label(egui::RichText::new("⇄").size(14.0).color(semi::TEXT_3));
                ui.label(
                    egui::RichText::new(format!(
                        "{} · 房间 {}",
                        short_server(&self.server),
                        self.workspace
                    ))
                    .size(13.0)
                    .color(semi::TEXT_2),
                );
            });

            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("{} · {}", self.mode.label(), self.filter_summary()))
                    .size(12.0)
                    .color(semi::TEXT_3),
            );

            ui.add_space(12.0);

            if let Some((done, total, what)) = &self.progress {
                let frac = if *total == 0 {
                    0.0
                } else {
                    *done as f32 / *total as f32
                };
                ui.add(egui::ProgressBar::new(frac).show_percentage());
                ui.label(egui::RichText::new(what).size(11.0).color(semi::TEXT_3));
            } else if !self.issues().is_empty() {
                ui.label(
                    egui::RichText::new("配置不完整，无法同步")
                        .size(13.0)
                        .color(semi::DANGER),
                );
                for issue in self.issues() {
                    ui.label(
                        egui::RichText::new(format!("· {issue}"))
                            .size(12.0)
                            .color(semi::DANGER),
                    );
                }
            } else if let Some(res) = &self.last {
                match res {
                    Ok(r) if r.up_to_date => {
                        ui.label(
                            egui::RichText::new("两端内容一致")
                                .size(13.0)
                                .color(semi::SUCCESS),
                        );
                    }
                    Ok(r) => {
                        ui.horizontal(|ui| {
                            stat(ui, "推送", r.pushed);
                            stat(ui, "拉取", r.pulled);
                            stat(ui, "删除", r.deleted);
                            stat(ui, "冲突", r.conflicts);
                        });
                        ui.label(
                            egui::RichText::new(format!(
                                "上行 {} · 下行 {}",
                                human_bytes(r.bytes_sent),
                                human_bytes(r.bytes_recv)
                            ))
                            .size(11.0)
                            .color(semi::TEXT_3),
                        );
                    }
                    Err(e) => {
                        ui.label(egui::RichText::new(e).size(12.0).color(semi::DANGER));
                    }
                }
            } else {
                ui.label(egui::RichText::new("尚未同步").size(13.0).color(semi::TEXT_3));
            }

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let can = !self.running && self.current().is_ready();
                let label = if self.running { "同步中…" } else { "立即同步" };
                let btn = egui::Button::new(egui::RichText::new(label).size(13.0).color(semi::WHITE))
                    .fill(semi::PRIMARY)
                    .rounding(egui::Rounding::same(6.0))
                    .min_size(egui::vec2(108.0, 32.0));
                if ui.add_enabled(can, btn).clicked() {
                    self.start_sync(120);
                }

                let btn2 = egui::Button::new(egui::RichText::new("设置").size(13.0))
                    .rounding(egui::Rounding::same(6.0))
                    .min_size(egui::vec2(72.0, 32.0));
                if ui.add(btn2).clicked() {
                    self.tab = Tab::Settings;
                }

                ui.add_space(6.0);
                ui.checkbox(&mut self.auto, egui::RichText::new("自动同步").size(12.0));
            });
        });

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("最近记录").size(13.0).strong());
            if ui
                .link(egui::RichText::new("查看全部").size(12.0))
                .clicked()
            {
                self.tab = Tab::Records;
            }
        });
        ui.add_space(6.0);

        if !self.records_loaded {
            self.reload_records();
        }
        if self.records.is_empty() {
            ui.label(
                egui::RichText::new("暂无记录。同步一次之后，每个被传输或删除的文件都会出现在这里。")
                    .size(12.0)
                    .color(semi::TEXT_3),
            );
        } else {
            for a in self.records.iter().take(6) {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(fmt_ts(a.created_at))
                            .size(11.0)
                            .color(semi::TEXT_3),
                    );
                    let color = if a.ok { semi::SUCCESS } else { semi::DANGER };
                    ui.label(egui::RichText::new(&a.action).size(11.0).color(color));
                    ui.label(
                        egui::RichText::new(&a.path)
                            .size(11.0)
                            .color(semi::TEXT)
                            .monospace(),
                    );
                });
            }
        }
    }

    fn filter_summary(&self) -> String {
        let f = self.filter();
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
            "不过滤".into()
        } else {
            parts.join(" · ")
        }
    }

    /// 设置页：连接、模式、过滤。所有配置项都在界面里，config.json 只是持久化载体。
    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);

        let card = egui::Frame::none()
            .fill(semi::WHITE)
            .stroke(egui::Stroke::new(1.0, semi::BORDER))
            .rounding(egui::Rounding::same(12.0))
            .inner_margin(egui::Margin::same(16.0));

        card.show(ui, |ui| {
            ui.label(egui::RichText::new("连接").size(13.0).strong());
            ui.add_space(8.0);

            egui::Grid::new("cfg")
                .num_columns(2)
                .spacing([10.0, 10.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("服务器").size(12.0).color(semi::TEXT_2));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.server)
                            .hint_text("ws://1.2.3.4:8443/ws")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label(egui::RichText::new("密钥").size(12.0).color(semi::TEXT_2));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.token)
                            .password(true)
                            .hint_text("与服务端 SYNC_TOKEN 一致")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label(egui::RichText::new("房间").size(12.0).color(semi::TEXT_2));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.workspace)
                            .hint_text("两台设备必须相同")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label(egui::RichText::new("同步目录").size(12.0).color(semi::TEXT_2));
                    ui.horizontal(|ui| {
                        let w = (ui.available_width() - 80.0).max(120.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.root)
                                .hint_text(r"D:/Sync")
                                .desired_width(w),
                        );
                        if ui.button("浏览…").clicked() {
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                self.root = dir.to_string_lossy().to_string();
                            }
                        }
                    });
                    ui.end_row();
                });

            ui.add_space(14.0);
            ui.label(egui::RichText::new("同步模式").size(13.0).strong());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.mode, SyncMode::Bidirectional, "双向");
                ui.radio_value(&mut self.mode, SyncMode::UploadOnly, "仅上传");
                ui.radio_value(&mut self.mode, SyncMode::DownloadOnly, "仅下载");
            });
            ui.label(
                egui::RichText::new(match self.mode {
                    SyncMode::Bidirectional => "两端都改，冲突时各留一份副本（常规用法）",
                    SyncMode::UploadOnly => "只把本机变更推给对端；单向场景下对端应设为「仅下载」",
                    SyncMode::DownloadOnly => "只接收对端变更；单向场景下对端应设为「仅上传」",
                })
                .size(11.0)
                .color(semi::TEXT_3),
            );

            ui.add_space(14.0);
            ui.label(egui::RichText::new("过滤规则").size(13.0).strong());
            ui.label(
                egui::RichText::new("两台设备必须完全一致，否则会中止同步")
                    .size(11.0)
                    .color(semi::TEXT_3),
            );
            ui.add_space(6.0);
            ui.checkbox(
                &mut self.filter_dotfiles,
                egui::RichText::new("忽略以 . 开头的文件和目录（如 .git）").size(12.0),
            );
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("忽略扩展名").size(12.0).color(semi::TEXT_2));
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter_ext)
                        .hint_text("lnk, pst, tmp")
                        .desired_width(180.0),
                );
                ui.label(egui::RichText::new("逗号分隔，不带点").size(11.0).color(semi::TEXT_3));
            });
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("大小范围").size(12.0).color(semi::TEXT_2));
                ui.add(
                    egui::DragValue::new(&mut self.filter_min_mb)
                        .speed(0.5)
                        .range(0.0..=1_000_000.0)
                        .suffix(" MB"),
                );
                ui.label(egui::RichText::new("—").size(12.0).color(semi::TEXT_3));
                ui.add(
                    egui::DragValue::new(&mut self.filter_max_mb)
                        .speed(0.5)
                        .range(0.0..=1_000_000.0)
                        .suffix(" MB"),
                );
                ui.label(egui::RichText::new("0 表示不限").size(11.0).color(semi::TEXT_3));
            });

            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let btn = egui::Button::new(
                    egui::RichText::new("保存配置").size(13.0).color(semi::WHITE),
                )
                .fill(semi::PRIMARY)
                .rounding(egui::Rounding::same(6.0))
                .min_size(egui::vec2(96.0, 32.0));
                if ui.add(btn).clicked() {
                    let issues = self.issues();
                    if !issues.is_empty() {
                        self.saved_hint = Some("未保存：配置不完整".to_string());
                        self.log(format!("拒绝保存：{}", issues.join("；")));
                    } else {
                        match self.current().save() {
                            Ok(()) => {
                                self.cfg = self.current();
                                self.saved_hint = Some("已保存".to_string());
                                self.log("配置已保存");
                            }
                            Err(e) => self.log(format!("保存失败: {e}")),
                        }
                    }
                }
                if let Some(h) = &self.saved_hint {
                    ui.label(egui::RichText::new(h).size(12.0).color(semi::TEXT_3));
                }
            });
        });
    }

    fn ui_records(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("文件记录").strong());
            ui.label(
                egui::RichText::new(format!("最近 {} 条", self.records.len()))
                    .weak()
                    .small(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("打开数据目录").clicked() {
                    let dir = Config::dir();
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                }
                if ui.button("清空").clicked() {
                    match Store::open(&Config::state_db()).and_then(|s| s.clear_activities()) {
                        Ok(()) => {
                            self.records.clear();
                            self.log("已清空文件记录");
                        }
                        Err(e) => self.log(format!("清空记录失败: {e}")),
                    }
                }
                if ui.button("刷新").clicked() {
                    self.reload_records();
                }
            });
        });
        ui.add_space(4.0);
        ui.separator();

        if self.records.is_empty() {
            ui.add_space(10.0);
            ui.label(egui::RichText::new("还没有记录。同步一次之后，每个被传输或删除的文件都会出现在这里。").weak());
            return;
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("records")
                    .num_columns(4)
                    .striped(true)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        for a in &self.records {
                            ui.label(egui::RichText::new(fmt_ts(a.created_at)).small().weak());
                            let color = if a.ok {
                                egui::Color32::from_rgb(76, 175, 125)
                            } else {
                                egui::Color32::from_rgb(226, 75, 74)
                            };
                            ui.label(egui::RichText::new(&a.action).small().color(color));
                            ui.label(egui::RichText::new(&a.path).small().monospace());
                            ui.label(egui::RichText::new(&a.message).small().weak());
                            ui.end_row();
                        }
                    });
            });
    }

    fn ui_logs(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("运行日志").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("打开数据目录").clicked() {
                    let dir = Config::dir();
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                }
            });
        });
        ui.add_space(4.0);
        ui.separator();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.logs {
                    ui.label(egui::RichText::new(line).small().monospace());
                }
            });
    }
}

fn stat(ui: &mut egui::Ui, label: &str, n: usize) {
    ui.label(egui::RichText::new(label).size(12.0).color(semi::TEXT_3));
    ui.label(
        egui::RichText::new(n.to_string())
            .size(14.0)
            .strong()
            .color(semi::TEXT),
    );
    ui.add_space(12.0);
}

fn short_server(s: &str) -> String {
    s.trim()
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .trim_end_matches("/ws")
        .to_string()
}

fn human_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    let f = n as f64;
    if f < KB {
        format!("{n} B")
    } else if f < KB * KB {
        format!("{:.1} KB", f / KB)
    } else if f < KB * KB * KB {
        format!("{:.1} MB", f / (KB * KB))
    } else {
        format!("{:.2} GB", f / (KB * KB * KB))
    }
}

/// 本地时间的简短表示，只用于界面显示。
fn now_stamp() -> String {
    let rem = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        .rem_euclid(86400);
    format!("{:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

/// 记录里的时间戳：月-日 时:分（本地时区按 UTC+8 估算，仅用于界面）
fn fmt_ts(secs: i64) -> String {
    let local = secs + 8 * 3600;
    let days = local.div_euclid(86400);
    let rem = local.rem_euclid(86400);
    let (_, m, d) = civil(days);
    format!(
        "{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// 写日志用的完整时间戳（UTC，精确到秒）。
fn stamp_full() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
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
