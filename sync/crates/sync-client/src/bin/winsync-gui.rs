//! WinSync 图形界面。
//!
//! 这是给人用的主程序：填好服务器、密钥、同步目录，点一次保存，之后开着就行。
//! 同步在后台线程跑（独立的 tokio runtime），界面通过 channel 收事件，不会被阻塞。
//!
//! release 编译时用 `windows_subsystem = "windows"` 隐藏黑色控制台窗口。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;
use notify::Watcher;
use sync_client::session::{self, Event, Report};
use sync_client::Config;

enum WorkerMsg {
    Event(Event),
    Done(Result<Report, String>),
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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([580.0, 720.0])
            .with_min_inner_size([520.0, 560.0])
            .with_title(format!("WinSync v{} — 文件夹同步", env!("CARGO_PKG_VERSION"))),
        ..Default::default()
    };

    eframe::run_native(
        "WinSync",
        options,
        Box::new(|cc| {
            setup_fonts(&cc.egui_ctx);
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

struct App {
    cfg: Config,
    server: String,
    token: String,
    workspace: String,
    root: String,

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
        let mut app = Self {
            server: cfg.server.clone(),
            token: cfg.token.clone(),
            workspace: cfg.workspace.clone(),
            root: cfg.root.clone(),
            cfg,
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
        };

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

        v
    }

    fn current(&self) -> Config {
        Config {
            server: self.server.trim().to_string(),
            token: self.token.trim().to_string(),
            workspace: self.workspace.trim().to_string(),
            root: self.root.trim().to_string(),
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

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
        // 两个来源：目录有变更（去抖 3 秒），或 30 秒兜底轮询。
        // 轮询不能去掉——notify 在休眠唤醒、缓冲区溢出、网络盘场景下会丢事件。
        let due_poll = Instant::now() >= self.next_auto;
        let due_change =
            self.dirty && self.last_change.elapsed() > Duration::from_secs(3);

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

        egui::TopBottomPanel::top("head").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("WinSync");
                ui.label(
                    egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                        .weak()
                        .small(),
                );
                ui.label(egui::RichText::new("两台 Windows 经 Linux 中继同步").weak());
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("连接设置").strong());
            ui.add_space(4.0);

            egui::Grid::new("cfg")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    ui.label("服务器");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.server)
                            .hint_text("ws://1.2.3.4:8443/ws")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("密钥");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.token)
                            .password(true)
                            .hint_text("与服务端 SYNC_TOKEN 一致")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("房间");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.workspace)
                            .hint_text("两台设备必须相同")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("同步目录");
                    ui.horizontal(|ui| {
                        let w = (ui.available_width() - 76.0).max(120.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.root)
                                .hint_text(r"D:\Sync")
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

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("保存配置").clicked() {
                    // 配置不完整时拒绝保存。之前允许存下 root 为空的配置，
                    // 用户下次启动就会静默不工作，极难排查。
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

                let can_run = !self.running && self.current().is_ready();
                if ui
                    .add_enabled(can_run, egui::Button::new("立即同步"))
                    .clicked()
                {
                    self.start_sync(120);
                }

                if self.running {
                    ui.spinner();
                    ui.label(egui::RichText::new("同步中…").weak());
                }

                ui.separator();
                ui.checkbox(&mut self.auto, "自动同步（每 30 秒）");

                if let Some(h) = &self.saved_hint {
                    ui.label(egui::RichText::new(h).weak());
                }
            });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);

            // 状态区
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("状态").strong());
                if let Some(t) = &self.last_time {
                    ui.label(egui::RichText::new(format!("上次 {t}")).weak());
                }
            });
            ui.add_space(4.0);

            if let Some((done, total, what)) = &self.progress {
                let frac = if *total == 0 {
                    0.0
                } else {
                    *done as f32 / *total as f32
                };
                ui.add(egui::ProgressBar::new(frac).show_percentage());
                ui.label(egui::RichText::new(what).small().weak());
            } else if !self.issues().is_empty() {
                // 配置有问题时，直接把原因摆出来，而不是让用户对着"尚未同步"猜
                ui.label(
                    egui::RichText::new("配置不完整，无法同步：")
                        .color(egui::Color32::from_rgb(226, 75, 74)),
                );
                for issue in self.issues() {
                    ui.label(
                        egui::RichText::new(format!("· {issue}"))
                            .small()
                            .color(egui::Color32::from_rgb(226, 75, 74)),
                    );
                }
            } else if let Some(res) = &self.last {
                match res {
                    Ok(r) if r.up_to_date => {
                        ui.label(egui::RichText::new("✓ 两端内容一致").color(egui::Color32::from_rgb(76, 175, 125)));
                    }
                    Ok(r) => {
                        ui.label(format!(
                            "推送 {}　拉取 {}　删除 {}　冲突 {}",
                            r.pushed, r.pulled, r.deleted, r.conflicts
                        ));
                        ui.label(
                            egui::RichText::new(format!(
                                "上行 {}　下行 {}",
                                human_bytes(r.bytes_sent),
                                human_bytes(r.bytes_recv)
                            ))
                            .weak(),
                        );
                    }
                    Err(e) => {
                        ui.label(egui::RichText::new(format!("✗ {e}")).color(egui::Color32::from_rgb(226, 75, 74)));
                    }
                }
            } else {
                ui.label(egui::RichText::new("尚未同步").weak());
            }

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(4.0);
            ui.label(egui::RichText::new("日志").strong());

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.logs {
                        ui.label(egui::RichText::new(line).small().monospace());
                    }
                });
        });

        if self.running || self.auto {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
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
