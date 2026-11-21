//! WinSync 图形界面。
//!
//! 组织方式参考飞牛同步：
//!   任务 —— 一个任务 = 本机目录 ⇄ 服务器房间，可多个、可单独暂停
//!   记录 —— 文件级操作记录，按任务归档，可导出
//!   日志 —— 原始运行日志，排查用
//!   设置 —— 选中任务的连接、模式、过滤规则
//!
//! 同步在后台线程跑（每次会话一个独立 tokio runtime），界面通过 channel 收事件。
//! 托盘在自己的线程里泵 Windows 消息，不依赖界面刷新——之前把菜单事件挂在
//! update() 里，窗口一隐藏就点不动了。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use notify::Watcher;
use sync_client::config::{new_id, Settings, SyncMode};
use sync_client::session::{self, Event, Report};
use sync_client::Config;
use sync_core::{Activity, Filter, Store};

enum WorkerMsg {
    Event { task: String, ev: Event },
    Done { task: String, res: Result<Report, String> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Tasks,
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

/// 每个任务的运行时状态（不持久化）。
struct Runtime {
    running: bool,
    next_auto: Instant,
    progress: Option<(u64, u64, String)>,
    last: Option<Result<Report, String>>,
    last_time: Option<String>,
    logs: Vec<String>,
    /// 目录监听器必须一直持有，drop 掉就失效
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

/// 界面上正在编辑的字段（选中任务的副本），保存时才写回任务。
struct EditBuf {
    name: String,
    server: String,
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
            server: t.server.clone(),
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
        t.server = self.server.trim().to_string();
        t.token = self.token.trim().to_string();
        t.workspace = self.workspace.trim().to_string();
        t.root = self.root.trim().to_string();
        t.mode = self.mode;
        t.filter = self.filter();
    }
}

/// 新建任务向导：位置 → 连接 → 选项。
struct Wizard {
    step: u8,
    name: String,
    root: String,
    server: String,
    token: String,
    workspace: String,
    mode: SyncMode,
    dotfiles: bool,
    ext: String,
}

impl Wizard {
    fn new() -> Self {
        Self {
            step: 0,
            name: String::new(),
            root: String::new(),
            server: "ws://".to_string(),
            token: String::new(),
            workspace: "default".to_string(),
            mode: SyncMode::Bidirectional,
            dotfiles: false,
            ext: String::new(),
        }
    }

    fn to_task(&self) -> Config {
        let mut t = Config {
            id: new_id(),
            name: self.name.trim().to_string(),
            server: self.server.trim().to_string(),
            token: self.token.trim().to_string(),
            workspace: if self.workspace.trim().is_empty() {
                "default".to_string()
            } else {
                self.workspace.trim().to_string()
            },
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
        if t.name.is_empty() {
            t.name = t.display_name();
        }
        t
    }
}

const MB: f64 = 1024.0 * 1024.0;

fn main() -> eframe::Result<()> {
    // 单实例：两个实例会共用同一份状态库，拿到相同 device_id，配对后互相死锁。
    #[cfg(windows)]
    if !acquire_single_instance() {
        rfd::MessageDialog::new()
            .set_title("WinSync")
            .set_description("WinSync 已经在运行了。\n请在任务栏或托盘里找到已有的窗口。")
            .set_level(rfd::MessageLevel::Info)
            .show();
        return Ok(());
    }

    // 托盘线程先起来：它自己泵消息，不依赖界面刷新。
    spawn_tray();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([720.0, 780.0])
        .with_min_inner_size([600.0, 600.0])
        .with_title(window_title());

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

/// 命名互斥体实现单实例。句柄故意不关闭——进程退出时系统回收。
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
    let candidates = [
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\deng.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\simsun.ttc",
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

fn show_main_window() {
    if let Some(ctx) = CTX.get() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
        };
        let wide: Vec<u16> = window_title()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hwnd = FindWindowW(std::ptr::null(), wide.as_ptr());
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
    }
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
        let built = tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("WinSync — 文件夹同步")
            .with_icon(icon)
            .build();

        let Ok(_tray) = built else {
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

/// 从打包进二进制的 .ico 里取出 64×64 那档，转成 RGBA。
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
        let w = if bytes[e] == 0 {
            256
        } else {
            bytes[e] as usize
        };
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

// ---------------- App ----------------

struct App {
    settings: Settings,
    runtimes: Vec<Runtime>,
    selected: usize,
    tab: Tab,
    buf: EditBuf,
    auto: bool,
    wizard: Option<Wizard>,

    records: Vec<Activity>,
    records_loaded: bool,
    records_all: bool,
    records_for: String,

    saved_hint: Option<String>,

    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    watch_tx: Sender<String>,
    watch_rx: Receiver<String>,

    tray_ready: bool,
    quitting: bool,
    hidden_notice: bool,
}

impl App {
    fn new() -> Self {
        let (settings, load_err) = match Settings::load() {
            Ok(Some(s)) => (s, None),
            Ok(None) => (Settings::default(), None),
            Err(e) => (
                Settings::default(),
                Some(format!(
                    "读取配置失败：{e}\n已退回空配置，请检查 {}",
                    Config::file().display()
                )),
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

        let mut app = Self {
            settings,
            runtimes,
            selected: 0,
            tab: Tab::Tasks,
            buf,
            auto: true,
            wizard: None,
            records: Vec::new(),
            records_loaded: false,
            records_all: false,
            records_for: String::new(),
            saved_hint: None,
            tx,
            rx,
            watch_tx,
            watch_rx,
            tray_ready: false,
            quitting: false,
            hidden_notice: false,
        };

        let deadline = Instant::now() + Duration::from_millis(1000);
        while Instant::now() < deadline && !tray_ready() {
            std::thread::sleep(Duration::from_millis(20));
        }
        app.tray_ready = tray_ready();
        if !app.tray_ready {
            app.log(0, "托盘不可用：关闭窗口将直接退出");
        }
        if let Some(e) = load_err {
            app.log(0, e);
        }
        app
    }

    fn selected_task(&self) -> Option<&Config> {
        self.settings.tasks.get(self.selected)
    }

    /// 把编辑缓冲写回选中任务（不落盘）。
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
            if rt.logs.len() > 300 {
                rt.logs.remove(0);
            }
        }
    }

    fn add_task(&mut self, task: Config) {
        self.settings.tasks.push(task);
        self.runtimes.push(Runtime::new());
        let idx = self.settings.tasks.len() - 1;
        let _ = self.settings.save();
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
        // 状态库保留：误删任务后重建同名任务还能接上基线
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
        // 每次同步前落盘，避免"界面改了没保存、重启跑回旧值"
        let _ = self.settings.save();

        self.runtimes[i].running = true;
        self.runtimes[i].progress = None;
        self.log(i, format!("开始同步 {}", cfg.root));

        let tx = self.tx.clone();
        let id = cfg.id.clone();
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

            let res = rt.block_on(session::run(&cfg, Some(evt_tx), true, true, 25));
            let _ = fwd.join();
            let _ = tx.send(WorkerMsg::Done {
                task: cfg.id.clone(),
                res: res.map_err(|e| format!("{e:#}")),
            });
        });
    }

    fn task_index(&self, id: &str) -> Option<usize> {
        self.settings.tasks.iter().position(|t| t.id == id)
    }

    fn reload_records(&mut self) {
        self.records.clear();
        let want: Vec<String> = if self.records_all {
            self.settings.tasks.iter().map(|t| t.id.clone()).collect()
        } else {
            self.selected_task()
                .map(|t| vec![t.id.clone()])
                .unwrap_or_default()
        };
        let mut all: Vec<Activity> = Vec::new();
        for id in want {
            if let Some(i) = self.task_index(&id) {
                let db = self.settings.tasks[i].state_db();
                if let Ok(store) = Store::open(&db) {
                    if let Ok(mut list) = store.recent_activities(200) {
                        all.append(&mut list);
                    }
                }
            }
        }
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        all.truncate(300);
        self.records = all;
        self.records_loaded = true;
    }

    fn export_records(&mut self) {
        let path = Config::dir().join("records.csv");
        let mut out = String::from("time,action,ok,path,message\n");
        for a in &self.records {
            out.push_str(&format!(
                "{},{},{},{},{}\n",
                fmt_ts_full(a.created_at),
                a.action,
                if a.ok { "ok" } else { "fail" },
                csv_escape(&a.path),
                csv_escape(&a.message)
            ));
        }
        match std::fs::write(&path, out) {
            Ok(()) => {
                self.log(self.selected, format!("记录已导出到 {}", path.display()));
                let _ = std::process::Command::new("explorer")
                    .arg(Config::dir())
                    .spawn();
            }
            Err(e) => self.log(self.selected, format!("导出失败: {e}")),
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

        // 后台消息
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Event { task, ev } => {
                    if let Some(i) = self.task_index(&task) {
                        match ev {
                            Event::Log(s) => {
                                if let Some(rt) = self.runtimes.get_mut(i) {
                                    rt.logs.push(s);
                                    if rt.logs.len() > 300 {
                                        rt.logs.remove(0);
                                    }
                                }
                            }
                            Event::Progress { done, total, what } => {
                                if let Some(rt) = self.runtimes.get_mut(i) {
                                    rt.progress = Some((done, total, what));
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

        // 目录监听事件
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

        // 每个任务的监听与自动触发
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

        if self.tab == Tab::Records && !self.records_loaded {
            self.reload_records();
        }

        self.ui_header(ctx);
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Tasks => self.ui_tasks(ui),
            Tab::Records => self.ui_records(ui),
            Tab::Logs => self.ui_logs(ui),
            Tab::Settings => self.ui_settings(ui),
        });
        self.ui_wizard(ctx);

        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

impl App {
    fn ui_header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("head")
            .frame(
                egui::Frame::none()
                    .fill(semi::WHITE)
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (color, text) = if self.runtimes.iter().any(|r| r.running) {
                        (semi::WARN, "同步中")
                    } else if self.runtimes.iter().any(|r| matches!(r.last, Some(Err(_)))) {
                        (semi::DANGER, "有错误")
                    } else if self.settings.tasks.is_empty() {
                        (semi::TEXT_3, "无任务")
                    } else {
                        (semi::SUCCESS, "正常")
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
                        ui.checkbox(&mut self.auto, egui::RichText::new("自动同步").size(12.0));
                        let n_paused = self.settings.tasks.iter().filter(|t| t.paused).count();
                        if n_paused > 0 {
                            ui.label(
                                egui::RichText::new(format!("{n_paused} 个已暂停"))
                                    .size(11.0)
                                    .color(semi::TEXT_3),
                            );
                        }
                    });
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    for (tab, label) in [
                        (Tab::Tasks, "任务"),
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
    }

    fn ui_tasks(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let btn =
                egui::Button::new(egui::RichText::new("+ 新建任务").size(13.0).color(semi::WHITE))
                    .fill(semi::PRIMARY)
                    .rounding(egui::Rounding::same(6.0))
                    .min_size(egui::vec2(104.0, 30.0));
            if ui.add(btn).clicked() {
                self.wizard = Some(Wizard::new());
            }
            if ui.button("立即全部同步").clicked() {
                for i in 0..self.settings.tasks.len() {
                    self.start_sync(i);
                }
            }
        });
        ui.add_space(10.0);

        if self.settings.tasks.is_empty() {
            ui.label(
                egui::RichText::new(
                    "还没有同步任务。点「+ 新建任务」，选一个本地目录和一台服务器即可。",
                )
                .size(13.0)
                .color(semi::TEXT_2),
            );
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
                    let (running, paused, last_err) = {
                        let rt = &self.runtimes[i];
                        (rt.running, task.paused, matches!(rt.last, Some(Err(_))))
                    };
                    let (progress, last, last_time) = {
                        let rt = &self.runtimes[i];
                        (rt.progress.clone(), rt.last.clone(), rt.last_time.clone())
                    };

                    let frame = egui::Frame::none()
                        .fill(semi::WHITE)
                        .stroke(egui::Stroke::new(
                            if selected { 1.5 } else { 1.0 },
                            if selected { semi::PRIMARY } else { semi::BORDER },
                        ))
                        .rounding(egui::Rounding::same(12.0))
                        .inner_margin(egui::Margin::same(14.0));

                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (color, label) = if running {
                                (semi::WARN, "同步中")
                            } else if paused {
                                (semi::TEXT_3, "已暂停")
                            } else if last_err {
                                (semi::DANGER, "有错误")
                            } else {
                                (semi::SUCCESS, "正常")
                            };
                            ui.label(
                                egui::RichText::new(task.display_name())
                                    .size(15.0)
                                    .strong()
                                    .color(semi::TEXT),
                            );
                            ui.label(egui::RichText::new("●").size(10.0).color(color));
                            ui.label(egui::RichText::new(label).size(11.0).color(color));

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .button(egui::RichText::new("删除").size(11.0))
                                        .clicked()
                                    {
                                        remove = Some(i);
                                    }
                                    if ui
                                        .button(egui::RichText::new(if paused {
                                            "恢复"
                                        } else {
                                            "暂停"
                                        })
                                        .size(11.0))
                                        .clicked()
                                    {
                                        pause = Some(i);
                                    }
                                    let can = !running && task.issues().is_empty();
                                    let b = egui::Button::new(
                                        egui::RichText::new(if running {
                                            "同步中…"
                                        } else {
                                            "立即同步"
                                        })
                                        .size(11.0)
                                        .color(semi::WHITE),
                                    )
                                    .fill(semi::PRIMARY)
                                    .rounding(egui::Rounding::same(6.0))
                                    .min_size(egui::vec2(76.0, 26.0));
                                    if ui.add_enabled(can, b).clicked() {
                                        run = Some(i);
                                    }
                                },
                            );
                        });

                        ui.add_space(3.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "{}  ⇄  {} · 房间 {}",
                                if task.root.is_empty() {
                                    "未选目录"
                                } else {
                                    &task.root
                                },
                                short_server(&task.server),
                                task.workspace
                            ))
                            .size(12.0)
                            .color(semi::TEXT_2),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{} · {}",
                                task.mode.label(),
                                filter_summary(&task.filter)
                            ))
                            .size(11.0)
                            .color(semi::TEXT_3),
                        );

                        if let Some((done, total, what)) = &progress {
                            let frac = if *total == 0 {
                                0.0
                            } else {
                                *done as f32 / *total as f32
                            };
                            ui.add(egui::ProgressBar::new(frac).show_percentage());
                            ui.label(egui::RichText::new(what).size(11.0).color(semi::TEXT_3));
                        } else if let Some(res) = &last {
                            match res {
                                Ok(r) if r.up_to_date => {
                                    ui.label(
                                        egui::RichText::new("两端内容一致")
                                            .size(11.0)
                                            .color(semi::SUCCESS),
                                    );
                                }
                                Ok(r) => {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "推送 {} · 拉取 {} · 删除 {} · 冲突 {} · 上行 {} · 下行 {}",
                                            r.pushed,
                                            r.pulled,
                                            r.deleted,
                                            r.conflicts,
                                            human_bytes(r.bytes_sent),
                                            human_bytes(r.bytes_recv)
                                        ))
                                        .size(11.0)
                                        .color(semi::TEXT_2),
                                    );
                                }
                                Err(e) => {
                                    ui.label(
                                        egui::RichText::new(e).size(11.0).color(semi::DANGER),
                                    );
                                }
                            }
                        }

                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.link(egui::RichText::new("设置").size(11.0)).clicked() {
                                select = Some(i);
                            }
                            if let Some(t) = &last_time {
                                ui.label(
                                    egui::RichText::new(format!("上次同步 {t}"))
                                        .size(11.0)
                                        .color(semi::TEXT_3),
                                );
                            }
                        });
                    });
                    ui.add_space(10.0);
                }
            });

        if let Some(i) = select {
            self.select(i);
            self.tab = Tab::Settings;
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

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        if self.selected_task().is_none() {
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new("先新建一个任务，再来这里配置。")
                    .size(13.0)
                    .color(semi::TEXT_2),
            );
            return;
        }

        ui.add_space(8.0);
        let card = egui::Frame::none()
            .fill(semi::WHITE)
            .stroke(egui::Stroke::new(1.0, semi::BORDER))
            .rounding(egui::Rounding::same(12.0))
            .inner_margin(egui::Margin::same(16.0));

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                card.show(ui, |ui| {
                    ui.label(egui::RichText::new("任务").size(13.0).strong());
                    ui.add_space(8.0);
                    egui::Grid::new("cfg_task")
                        .num_columns(2)
                        .spacing([10.0, 10.0])
                        .show(ui, |ui| {
                            ui.label(pale("名称"));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.buf.name)
                                    .hint_text("留空则用目录名")
                                    .desired_width(f32::INFINITY),
                            );
                            ui.end_row();

                            ui.label(pale("服务器"));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.buf.server)
                                    .hint_text("ws://1.2.3.4:8443/ws")
                                    .desired_width(f32::INFINITY),
                            );
                            ui.end_row();

                            ui.label(pale("密钥"));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.buf.token)
                                    .password(true)
                                    .hint_text("与服务端 SYNC_TOKEN 一致")
                                    .desired_width(f32::INFINITY),
                            );
                            ui.end_row();

                            ui.label(pale("房间"));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.buf.workspace)
                                    .hint_text("两台设备必须相同")
                                    .desired_width(f32::INFINITY),
                            );
                            ui.end_row();

                            ui.label(pale("同步目录"));
                            ui.horizontal(|ui| {
                                let w = (ui.available_width() - 80.0).max(120.0);
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.buf.root)
                                        .hint_text(r"D:\Sync")
                                        .desired_width(w),
                                );
                                if ui.button("浏览…").clicked() {
                                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                        self.buf.root = dir.to_string_lossy().to_string();
                                    }
                                }
                            });
                            ui.end_row();
                        });

                    ui.add_space(14.0);
                    ui.label(egui::RichText::new("同步模式").size(13.0).strong());
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.buf.mode, SyncMode::Bidirectional, "双向");
                        ui.radio_value(&mut self.buf.mode, SyncMode::UploadOnly, "仅上传");
                        ui.radio_value(&mut self.buf.mode, SyncMode::DownloadOnly, "仅下载");
                    });
                    ui.label(
                        egui::RichText::new(match self.buf.mode {
                            SyncMode::Bidirectional => "两端都改，冲突时各留一份副本（常规用法）",
                            SyncMode::UploadOnly => {
                                "只把本机变更推给对端；单向场景下对端应设为「仅下载」"
                            }
                            SyncMode::DownloadOnly => {
                                "只接收对端变更；单向场景下对端应设为「仅上传」"
                            }
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
                        &mut self.buf.dotfiles,
                        egui::RichText::new("忽略以 . 开头的文件和目录（如 .git）").size(12.0),
                    );
                    ui.horizontal(|ui| {
                        ui.label(pale("忽略扩展名"));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.buf.ext)
                                .hint_text("lnk, pst, tmp")
                                .desired_width(180.0),
                        );
                        ui.label(
                            egui::RichText::new("逗号分隔，不带点").size(11.0).color(semi::TEXT_3),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label(pale("大小范围"));
                        ui.add(
                            egui::DragValue::new(&mut self.buf.min_mb)
                                .speed(0.5)
                                .range(0.0..=1_000_000.0)
                                .suffix(" MB"),
                        );
                        ui.label(egui::RichText::new("—").size(12.0).color(semi::TEXT_3));
                        ui.add(
                            egui::DragValue::new(&mut self.buf.max_mb)
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
                            self.apply_buf();
                            let issues = self
                                .selected_task()
                                .map(|t| t.issues())
                                .unwrap_or_default();
                            if issues.is_empty() {
                                match self.settings.save() {
                                    Ok(()) => {
                                        self.saved_hint = Some("已保存".into());
                                        self.log(self.selected, "配置已保存");
                                    }
                                    Err(e) => {
                                        self.log(self.selected, format!("保存失败: {e}"))
                                    }
                                }
                            } else {
                                self.saved_hint = Some("未保存：配置不完整".into());
                                self.log(
                                    self.selected,
                                    format!("拒绝保存：{}", issues.join("；")),
                                );
                            }
                        }
                        if let Some(h) = &self.saved_hint {
                            ui.label(egui::RichText::new(h).size(12.0).color(semi::TEXT_3));
                        }
                    });
                });

                if let Some(t) = self.selected_task() {
                    let issues = t.issues();
                    if !issues.is_empty() {
                        ui.add_space(10.0);
                        for i in issues {
                            ui.label(
                                egui::RichText::new(format!("· {i}"))
                                    .size(12.0)
                                    .color(semi::DANGER),
                            );
                        }
                    }
                }
            });
    }

    fn ui_records(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("同步记录").size(14.0).strong());
            if ui
                .checkbox(&mut self.records_all, egui::RichText::new("全部任务").size(12.0))
                .changed()
            {
                self.records_for = if self.records_all { "*".into() } else { String::new() };
                self.records_loaded = false;
            }
            if !self.records_all {
                let name = self
                    .selected_task()
                    .map(|t| t.display_name())
                    .unwrap_or_default();
                ui.label(egui::RichText::new(name).size(11.0).color(semi::TEXT_3));
            }
            ui.label(
                egui::RichText::new(format!("{} 条", self.records.len()))
                    .size(11.0)
                    .color(semi::TEXT_3),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("导出 CSV").clicked() {
                    self.export_records();
                }
                if ui.button("清空").clicked() {
                    let ids: Vec<String> = if self.records_all {
                        self.settings.tasks.iter().map(|t| t.id.clone()).collect()
                    } else {
                        self.selected_task()
                            .map(|t| vec![t.id.clone()])
                            .unwrap_or_default()
                    };
                    for id in ids {
                        if let Some(i) = self.task_index(&id) {
                            let db = self.settings.tasks[i].state_db();
                            if let Ok(store) = Store::open(&db) {
                                let _ = store.clear_activities();
                            }
                        }
                    }
                    self.records.clear();
                    self.log(self.selected, "已清空记录");
                }
                if ui.button("刷新").clicked() {
                    self.reload_records();
                }
            });
        });
        ui.add_space(6.0);
        ui.separator();

        if self.records.is_empty() {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(
                    "还没有记录。同步一次之后，每个被传输或删除的文件都会出现在这里。",
                )
                .size(12.0)
                .color(semi::TEXT_3),
            );
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
                            ui.label(
                                egui::RichText::new(fmt_ts(a.created_at))
                                    .size(11.0)
                                    .color(semi::TEXT_3),
                            );
                            let color = if a.ok { semi::SUCCESS } else { semi::DANGER };
                            ui.label(egui::RichText::new(&a.action).size(11.0).color(color));
                            ui.label(egui::RichText::new(&a.path).size(11.0).monospace());
                            ui.label(
                                egui::RichText::new(&a.message)
                                    .size(11.0)
                                    .color(semi::TEXT_3),
                            );
                            ui.end_row();
                        }
                    });
            });
    }

    fn ui_logs(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("运行日志").size(14.0).strong());
            if !self.settings.tasks.is_empty() {
                let name = self
                    .settings
                    .tasks
                    .get(self.selected)
                    .map(|t| t.display_name())
                    .unwrap_or_default();
                ui.label(egui::RichText::new(name).size(11.0).color(semi::TEXT_3));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("打开数据目录").clicked() {
                    let dir = Config::dir();
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                }
            });
        });
        ui.add_space(6.0);
        ui.separator();

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
                        egui::RichText::new("还没有日志。").size(12.0).color(semi::TEXT_3),
                    );
                }
                for line in &lines {
                    ui.label(egui::RichText::new(line).size(11.0).monospace());
                }
            });
    }

    /// 新建任务向导：位置 → 连接 → 选项
    fn ui_wizard(&mut self, ctx: &egui::Context) {
        let Some(w) = self.wizard.as_mut() else {
            return;
        };
        let mut close = false;
        let mut finish: Option<Config> = None;

        egui::Window::new("新建同步任务")
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("第 {} / 3 步", w.step + 1))
                        .size(11.0)
                        .color(semi::TEXT_3),
                );
                ui.add_space(8.0);

                match w.step {
                    0 => {
                        ui.label(egui::RichText::new("选择要同步的本地目录").size(13.0).strong());
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut w.root)
                                    .hint_text(r"D:\Sync")
                                    .desired_width(300.0),
                            );
                            if ui.button("浏览…").clicked() {
                                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                    w.root = dir.to_string_lossy().to_string();
                                }
                            }
                        });
                        ui.add_space(8.0);
                        ui.label(pale("任务名（留空则用目录名）"));
                        ui.add(
                            egui::TextEdit::singleline(&mut w.name)
                                .hint_text("例如：工作文档")
                                .desired_width(f32::INFINITY),
                        );
                    }
                    1 => {
                        ui.label(egui::RichText::new("连接到中继服务器").size(13.0).strong());
                        ui.add_space(6.0);
                        ui.label(pale("服务器地址"));
                        ui.add(
                            egui::TextEdit::singleline(&mut w.server)
                                .hint_text("ws://192.168.3.222:18443/ws")
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(8.0);
                        ui.label(pale("密钥"));
                        ui.add(
                            egui::TextEdit::singleline(&mut w.token)
                                .password(true)
                                .hint_text("与服务端 SYNC_TOKEN 一致")
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(8.0);
                        ui.label(pale("房间（两台设备必须相同）"));
                        ui.add(
                            egui::TextEdit::singleline(&mut w.workspace)
                                .hint_text("default")
                                .desired_width(f32::INFINITY),
                        );
                    }
                    _ => {
                        ui.label(egui::RichText::new("同步方式与过滤规则").size(13.0).strong());
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut w.mode, SyncMode::Bidirectional, "双向");
                            ui.radio_value(&mut w.mode, SyncMode::UploadOnly, "仅上传");
                            ui.radio_value(&mut w.mode, SyncMode::DownloadOnly, "仅下载");
                        });
                        ui.add_space(8.0);
                        ui.checkbox(
                            &mut w.dotfiles,
                            egui::RichText::new("忽略以 . 开头的文件和目录").size(12.0),
                        );
                        ui.horizontal(|ui| {
                            ui.label(pale("忽略扩展名"));
                            ui.add(
                                egui::TextEdit::singleline(&mut w.ext)
                                    .hint_text("lnk, pst, tmp")
                                    .desired_width(180.0),
                            );
                        });
                        ui.add_space(10.0);
                        let t = w.to_task();
                        let issues = t.issues();
                        if issues.is_empty() {
                            ui.label(
                                egui::RichText::new(format!(
                                    "将同步：{}  ⇄  {} · 房间 {}",
                                    t.root,
                                    short_server(&t.server),
                                    t.workspace
                                ))
                                .size(11.0)
                                .color(semi::TEXT_2),
                            );
                        } else {
                            for i in issues {
                                ui.label(
                                    egui::RichText::new(format!("· {i}"))
                                        .size(11.0)
                                        .color(semi::DANGER),
                                );
                            }
                        }
                    }
                }

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if w.step > 0 && ui.button("上一步").clicked() {
                        w.step -= 1;
                    }
                    let last = w.step == 2;
                    let label = if last { "创建任务" } else { "下一步" };
                    let btn = egui::Button::new(
                        egui::RichText::new(label).size(13.0).color(semi::WHITE),
                    )
                    .fill(semi::PRIMARY)
                    .rounding(egui::Rounding::same(6.0))
                    .min_size(egui::vec2(96.0, 30.0));
                    if ui.add(btn).clicked() {
                        if last {
                            let task = w.to_task();
                            if task.issues().is_empty() {
                                finish = Some(task);
                            }
                        } else {
                            w.step += 1;
                        }
                    }
                    if ui.button("取消").clicked() {
                        close = true;
                    }
                });
            });

        if let Some(t) = finish {
            self.add_task(t);
            close = true;
        }
        if close {
            self.wizard = None;
        }
    }
}

fn jitter() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        % 7
}

fn pale(s: &str) -> egui::RichText {
    egui::RichText::new(s).size(12.0).color(semi::TEXT_2)
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
        "不过滤".into()
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

fn now_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    fmt_ts(secs)
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
