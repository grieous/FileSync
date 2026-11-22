# WinSync × DeepSeek Harness 插件：详细实施计划

> 已选定推荐方案：**Rust daemon + dsh 插件 UI**，界面挂为**全局主区面板**（`main` key=winsync），入口是**左侧栏图标行**（`sidebar.panellist`），并提供 agent 工具。
> 本文是实施蓝图：文件级拆分、接口契约、任务分解、验收标准。未开始写业务代码。

## 0. 方案边界（一次说清，后面不重复）

- 同步算法**零改动**：`sync-proto` / `sync-core` / `session::run` 原样复用。
- exe 保留，短期内不重构成 daemon 前端。**两者完全隔离**（用户硬要求，见 §2.8）：不共享目录、不共享互斥体、不互相启停，任一方可独立运行/升级/卸载。
- daemon 只写自己那棵树：`%LOCALAPPDATA%\dsh-winsync\{config.json, tasks\*.db, sync.log}`；绝不写 exe 的 `%APPDATA%\WinSync`。
- 界面为深色单主题（与 exe 一致），不跟随 dsh 浅色主题；浅色适配列在 P5。
- 托盘、原生文件对话框、像素级 1:1 不做（差异已在上一版文档列明）。

## 1. 交付物与仓库布局

```
sync/
├── Cargo.toml                      # + members: crates/sync-daemon
└── crates/
    ├── sync-daemon/                # 新增（约 700 行 Rust）
    │   ├── Cargo.toml
    │   └── src/
    │       ├── main.rs             # 参数、互斥体、daemon.json、axum 启动
    │       ├── state.rs            # AppState / TaskRuntime / 共享快照
    │       ├── engine.rs           # 自动同步调度 + worker 管理（从 GUI 迁移）
    │       ├── api.rs              # 路由与处理函数
    │       ├── events.rs           # SSE 广播（broadcast → stream）
    │       ├── auth.rs             # Bearer + Origin 校验
    │       ├── fsapi.rs            # /fs/list（root 内目录列举）
    │       ├── settings_api.rs     # 全局设置、日志清理、自启
    │       └── win.rs              # 互斥体 / 注册表 / 打开目录（cfg(windows)）
    └── sync-client/src/bin/winsync-gui.rs   # 不动（后续可选改为 daemon 前端）

dsh-winsync/                        # 新增 npm 包（独立于 Rust workspace）
├── package.json                    # name, dsh.bundle, dsh.client.inject, files
├── cordis.patch.yml                # insert 一行：id: winsync, name: dsh-winsync
├── icon.svg
├── tsconfig.json                   # 宿主半（Node ESM）
├── tsconfig.client.json            # 客户端（DOM + React）
├── scripts/build-client.mjs        # esbuild + ModuleLoader 包装
├── src/
│   ├── index.ts                    # 宿主入口：inject / apply
│   ├── daemon.ts                   # 发现、拉起、健康检查、退出
│   ├── proxy.ts                    # /winsync/api/* 转发（含 SSE 直通）
│   ├── tools.ts                    # 4 个 agent 工具
│   ├── prefs.ts                    # dsh-plugin.json 读写
│   └── client/
│       ├── index.tsx               # slots 注册入口
│       ├── App.tsx                 # 侧栏 + 内容路由
│       ├── pages/{Tasks,Logs,Settings}.tsx
│       ├── components/{TaskCard,Wizard,TaskEditor,RawLog,DataTable,Empty,Toast}.tsx
│       ├── store.ts                # 快照 + SSE 合并 + 动作
│       ├── api.ts                  # fetch 封装（同源 /winsync/api）
│       ├── theme.ts                # 设计令牌（与 ui_colors 同值）
│       └── app.css                 # ws- 前缀类，运行时注入
└── README.md                       # 安装、开发、与 exe 的差异
```

## 2. daemon 详细设计

### 2.1 crate 与依赖

```toml
[package]
name = "sync-daemon"
version.workspace = true
edition.workspace = true

[dependencies]
sync-client.workspace = true
sync-core.workspace = true
anyhow.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio.workspace = true
axum.workspace = true
futures-util.workspace = true
notify.workspace = true
uuid.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true

[target.'cfg(windows)'.dependencies]
windows-sys = { workspace = true }   # 需补 features：Threading/Registry/Console/Shell
```

> 注：workspace 里 `windows-sys` 目前写在 `sync-client` 的 target 段，daemon 需要同样一份；把版本提到 `[workspace.dependencies]` 避免重复。

### 2.2 运行时状态模型

```rust
pub struct AppState {
    pub boot: BootInfo,                  // version / pid / config_path / data_dir / token
    pub settings: RwLock<Settings>,      // 与 config.json 同构
    pub runtimes: RwLock<Vec<TaskRuntime>>, // 与 settings.tasks 等长、同序
    pub auto: AtomicBool,                // 自动同步总开关（界面上的 checkbox）
    pub events: broadcast::Sender<Event>,// SSE
    pub worker_tx: mpsc::UnboundedSender<WorkerMsg>,
    pub dirty_tx: mpsc::UnboundedSender<String>, // 目录变更（task id）
}

pub struct TaskRuntime {
    pub running: bool,
    pub progress: Option<Progress>,      // done / total / what
    pub last: Option<Report>,            // 复用 session::Report
    pub last_error: Option<String>,
    pub last_time: Option<i64>,          // Unix 秒
    pub next_auto_at: Instant,
    pub logs: VecDeque<String>,          // 上限 400，与 GUI 一致
    pub watcher: Option<notify::RecommendedWatcher>,
    pub watch_root: String,
    pub dirty: bool,
    pub last_change: Instant,
}
```

`Runtime` 的可见字段通过 `/state` 序列化给前端；`Report`/`Progress` 需要加 `Serialize`（`session.rs` 加 `#[derive(Serialize)]`，或用 daemon 侧 DTO 映射——**选 DTO**，避免动 session 的公开类型）。

### 2.3 调度算法（从 GUI `update()` 平移）

```
loop 每 250ms:
  1. 取 worker 消息：Event::Log/Progress → 更新 runtime、写 sync.log、广播 SSE
                     Done(Ok/Err) → running=false、last/last_time/last_error、广播 done + state
  2. 取 dirty 消息：标记 runtime.dirty=true、last_change=now
  3. 对每个任务（顺序执行，无重复启动）：
       if watch_root != root → 重建 watcher（recursive，忽略 Access 事件）
       if !auto || paused || running → continue
       due_poll   = now >= next_auto_at
       due_change = dirty && now-last_change > 3s
       if (due_poll || due_change) && issues.is_empty():
           next_auto_at = now + 28..34s（随机抖动）
           dirty = false
           start_sync(task)
  4. 若距上次 state 广播 > 1s 且有变化 → 广播 state
```

`start_sync`：

```
校验 issues（不通过 → 直接返回错误，界面红字）
settings.save()（原子写）
runtime.running = true
tokio::spawn:
    session::run(&cfg, Some(evt_tx), true, true, 25, limits)
    把 Event 经 std mpsc 转成 WorkerMsg（沿用 GUI 的转发线程写法）
```

### 2.4 HTTP API 契约

鉴权：所有 `/api/*` 需要 `Authorization: Bearer <boot.token>`；`/health` 免鉴权但只返回 `{ok,version,pid}`。

| 方法 | 路径 | 请求 | 响应 |
|---|---|---|---|
| GET | `/health` | — | `{ok:true,version:"0.6.2",pid:1234}` |
| GET | `/api/state` | — | 见下 |
| POST | `/api/auto` | `{on:bool}` | `{ok:true,auto:bool}` |
| POST | `/api/tasks` | `TaskInput` | `{ok:true,task:TaskView}` |
| PATCH | `/api/tasks/{id}` | `TaskPatch`（字段可选；`token` 空=不改；换目录需 `force:true`） | `{ok:true,task}` 或 `409 root-changed` |
| DELETE | `/api/tasks/{id}` | — | `{ok:true}`（状态库保留） |
| POST | `/api/tasks/{id}/sync` | — | `202 {ok:true}`；失败原因 `409 not-ready` |
| POST | `/api/tasks/{id}/pause` \| `/resume` | — | `{ok:true}` |
| GET | `/api/activities?task=all\|<id>&status=all\|ok\|fail&q=&limit=500` | — | `{rows:[ActivityView],total}` |
| DELETE | `/api/activities?task=all\|<id>` | — | `{ok:true,deleted:n}` |
| GET | `/api/export.csv?task=all\|<id>` | — | `text/csv; charset=utf-8`（表头同 exe） |
| GET | `/api/logs?task=<id>&limit=400` | — | `{lines:[string]}`（环形缓冲） |
| GET | `/api/logfile?lines=200` | — | `{lines:[string]}`（`sync.log` 尾部） |
| POST | `/api/prune` | — | `{ok:true}`（按设置清理 + `VACUUM` 可选） |
| POST | `/api/autostart` | `{on:bool}` | `{ok:true,on:bool}` |
| POST | `/api/open-dir` | `{task?:id,path?:string}` | `{ok:true}`（path 必须等于某任务 root） |
| GET | `/api/fs/list?task=<id>&path=<rel>` | — | `{dirs:[{name,path}],parent?}`，仅目录名 |
| GET | `/api/events` | — | SSE |

`GET /api/state` 响应（**不含 token**）：

```json
{
  "version": "0.6.2",
  "pid": 1234,
  "data_dir": "C:\\Users\\gjn86\\AppData\\Roaming\\WinSync",
  "device": "DESKTOP-XXXX",
  "auto": true,
  "settings": { "auto_update": true, "log_keep_count": 0, "log_keep_days": 30,
                "limit_up_kbps": 64, "limit_down_kbps": 64 },
  "tasks": [{
    "id": "ad1c78a5", "name": "Sync", "display_name": "Sync",
    "server": "ws://192.168.3.222:18443/ws", "server_short": "192.168.3.222:18443",
    "workspace": "default", "root": "D:\\Sync", "has_token": true,
    "mode": "bidirectional",
    "filter": { "ignore_dotfiles": false, "ignore_extensions": [], "size_min": null, "size_max": null },
    "filter_summary": "无过滤", "filter_sig": "",
    "paused": false, "issues": [],
    "runtime": { "running": false, "progress": null,
                 "last": { "pushed": 3, "pulled": 1, "deleted": 0, "conflicts": 0,
                           "bytes_sent": 20480, "bytes_recv": 8192, "up_to_date": false },
                 "last_error": null, "last_time": 1732000000, "next_auto_in": 12 }
  }]
}
```

### 2.5 SSE 协议

```
event: hello      data: {"version":"0.6.2"}
event: log        data: {"task":"ad1c78a5","line":"上传 a.txt"}
event: progress   data: {"task":"ad1c78a5","done":3,"total":8,"what":"上传 b.bin"}
event: done       data: {"task":"ad1c78a5","report":{...}}          // 或 {"error":"..."}
event: state      data: {"tasks":[...],"auto":true}                 // 精简态，仅结构变化/暂停/设置时
: keepalive                                                          // 每 15s
```

实现：`tokio::sync::broadcast::Sender<Event>` + `futures_util::stream::unfold` 生成 `axum::body::Body::from_stream`；连接断开自动回收。进度事件按任务 200ms 节流，避免刷爆。

### 2.6 单实例、启动与退出

```
main:
  init tracing（写自己的 sync.log 同时 stderr）
  CreateMutexW("Local\\dsh-winsync-daemon-v1")        // 只锁自己，与 exe 的互斥体互不相干
    - 拿到 → 正常启动
    - ERROR_ALREADY_EXISTS → 读自己的 daemon.json
        * /health 有响应 → 打印 already-running，exit 0
        * pid 已死（陈旧握手）→ 接管：删文件后继续启动
  bind 127.0.0.1:0 → 取实际端口
  token = 32 字节随机 hex
  写 daemon.json {pid, port, token, version, started_at}（用户级 ACL，先写 .tmp 再 rename）
  axum::serve
  退出：删 daemon.json、保留日志
```

`daemon.json` 只给插件读，不进浏览器；插件用它拼 `Authorization`。

**不再检测、不再抢占 exe 的互斥体**：两者可以同时运行。唯一需要协同的是「同一目录不能被两边同时同步」，见 §2.8。

### 2.8 与 exe 的隔离契约（用户硬要求）

| 资源 | exe | daemon | 规则 |
|---|---|---|---|
| 配置 | `%APPDATA%\WinSync\config.json` | `%LOCALAPPDATA%\dsh-winsync\config.json` | 各写各的 |
| 状态库 | `%APPDATA%\WinSync\tasks\<id>.db` | `%LOCALAPPDATA%\dsh-winsync\tasks\<id>.db` | 各写各的 |
| 日志 | 上述目录的 `sync.log` | 上述目录的 `sync.log` | 各写各的 |
| 单实例锁 | exe 自己的 | `Local\dsh-winsync-daemon-v1` | 互不相干 |
| 进程 | 不被 daemon 启停 | 不被 exe 启停 | 各自独立 |
| 任务集 | exe 的任务 | 插件的任务 | 完全两套，不迁移、不同步 |

实现只需一处**纯增量**改动：`Config::dir()` 支持 `WINSYNC_STATE_DIR` 环境变量覆盖（daemon 启动时设为 `%LOCALAPPDATA%\dsh-winsync`）。exe 不设该变量，行为零变化；`WINSYNC_CONFIG` / `WINSYNC_DB` 已有覆盖能力，`Store::open(path)` 本来就收路径。

**唯一交集：目录重叠检测。** 同步是对文件的写操作，两个进程同步同一个目录必然互相删文件——这是历史上真出过的事故。所以：

- daemon 在「启动任务」和「每轮自动同步前」各查一次；
- 以**只读**方式读 exe 的 `config.json`（不写、不加锁、失败不影响 exe），比对 root 路径（规范化 + 大小写不敏感 + 互为父子目录都算重叠）；
- 命中 → 拒绝启动该任务，返回 `code=root-conflict`，文案点名是 exe 的哪个任务占用；
- 读不到 exe 配置且 `WinSync.exe` 正在运行 → 视为「无法确认」，同样拒绝，除非该任务被显式标记 `allowOverlap: true`（默认 false，界面里是折叠的高级项）。

这一条是唯一会让用户觉得「插件受限」的地方，但它是防止数据损坏的底线。

### 2.7 错误模型

统一 `{ok:false,error:{code,message}}`；code 取值：`not-ready` / `root-conflict` / `root-changed` / `bad-request` / `internal`。前端按 code 给不同提示，不做字符串匹配。

## 3. 插件宿主半

### 3.1 入口

```ts
export const name = 'dsh-winsync'
export const inject = ['webServer', 'tools']

export function apply(ctx: Context) {
  const prefs = loadPrefs()
  const daemon = createDaemonManager(prefs)          // 懒启动
  ctx.effect(() => ctx.webServer.register({
    kind: 'prefix', path: '/winsync/api',
    handler: (req, res) => proxy(daemon, req, res),  // 注入 Bearer、校验来源
  }))
  ctx.effect(() => ctx.webServer.register({
    kind: 'prefix', path: '/winsync/events',
    handler: (req, res) => proxySse(daemon, req, res),
  }))
  ctx.effect(() => registerTools(ctx.tools, daemon))
  ctx.effect(() => () => daemon.stopIfOwned(), 'dsh-winsync: daemon')
}
```

`inject` 只要 `webServer` + `tools`——不碰 `sessions`/`connection`，减少与 dsh 版本耦合；鉴权用 dsh 自身的同源策略（路由挂在 dsh 的 server 上，浏览器请求天然带会话）。

### 3.2 daemon 生命周期

```
ensure():
  1. 读 daemon.json → GET /health（300ms 超时）
  2. 通 → 复用（记录 owned=false，退出时不杀）
  3. 不通 → 找二进制：
        a. prefs.binPath（显式配置）
        b. %LOCALAPPDATA%\Programs\WinSync\winsync-daemon.exe
        c. PATH 中 winsync-daemon
      spawn（detached，hidden window），轮询 /health 最多 5s
  4. 失败 → 返回结构化错误给 UI：「未找到 winsync-daemon.exe，请先构建或指定路径」
```

`prefs.json`（`%APPDATA%\WinSync\dsh-plugin.json`）：`{binPath, stopDaemonOnExit:true, autoSpawn:true}`。默认退出即停（与「dsh 关了还在同步」的误解解耦）；要常驻就关掉该开关并开自启。

### 3.3 agent 工具

| 工具 | 入参 | 返回 | 说明 |
|---|---|---|---|
| `winsync_tasks` | — | 任务 id/名称/目录/模式/暂停/状态，密钥打码 | 只读 |
| `winsync_status` | `task?` | running、progress、上次结果、失败原因、下次自动同步倒计时 | 只读 |
| `winsync_sync_now` | `task?`, `wait?`(默认 false), `timeoutSec?`(≤120) | `{started:true}` 或等待后的 report | 有副作用（可能传播删除），描述里注明 |
| `winsync_recent_activity` | `task?`, `limit?`(≤200), `onlyFailed?` | path/action/ok/time | 只读，用于「刚才同步了什么」 |

工具返回统一 `{ok:true,data}` 文本化 JSON；错误返回 `{ok:false,error:{code,message}}`，不抛异常。
`sync_now` 加一句系统提示：调用前应先 `winsync_status` 确认没有正在运行的任务。

### 3.4 代理细节

- 只允许 `GET/POST/PATCH/DELETE`，其余 405。
- 请求体上限 256 KB（任务配置足够）。
- 注入 `Authorization` 与 `X-WinSync-Client: dsh-plugin`。
- SSE：不缓冲、不聚合，逐块 `pipe`；客户端断开即销毁上游连接。
- 上游 5xx → 502 + `{code:'daemon-unavailable'}`，前端显示「同步服务未启动」并给「重试」按钮。

## 4. 客户端

### 4.1 挂载

```tsx
export const name = 'dsh-winsync'
export const inject = ['slots']

export function apply(ctx: Context) {
  // 全局面板：不绑会话，首页也能进。左侧栏那一行按同一个 id 选中它。
  ctx.slots.inject('main', () =>
    ctx.slots.register({ name: 'main', key: 'winsync' }, () => <SyncApp />))
  ctx.slots.inject('sidebar.panellist', () =>
    ctx.slots.register({ name: 'sidebar.panellist', id: 'winsync', order: 10,
                         label: () => '同步' }, WinsyncIcon))
  ctx.slots.inject('settings.section', () =>
    ctx.slots.register({ name: 'settings.section', id: 'winsync', order: 60,
                         label: () => '文件夹同步' }, () => <SettingsSection />))
}
```

样式：`app.css` 以字符串导入，`ctx.effect` 插入 `<style data-winsync>`，卸载时移除；所有类名 `ws-` 前缀，避免污染 dsh。

### 4.2 组件树

```
SyncApp
├── Sidebar(180px)            Logo / 同步任务 / 同步日志 / 设置 / 底部「+ 创建同步任务」
├── Content
│   ├── TasksPage             标题 + 自动同步开关 + 设备行 + 任务卡列表
│   │   └── TaskCard          图标·名称·状态 / 进度条 / 三列信息 / 立即同步 / ⋯菜单 / 暂停圆钮
│   ├── LogsPage              任务下拉 + 状态下拉 + 搜索 + 表头 + 斑马纹 + 导出/清空
│   └── SettingsPage          自启 / 版本 / 日志清理 / 限速 / 诊断 / 当前任务
├── Wizard（整页，三步）       步骤条 + 表单 + 上一步/取消/下一步
├── TaskEditor（模态）         名称/服务器/密钥/房间/目录/模式/过滤/签名
└── RawLog（可拖动浮层）       环形日志，贴底自动滚动
```

### 4.3 数据流

```
useSyncStore():
  state      ← GET /winsync/api/state（进入页面 + 手动刷新）
  EventSource('/winsync/events')
      log      → 追加到 runtime.logs（上限 400）
      progress → 更新卡片进度条
      done     → 更新 last/last_time，刷新日志页（若在日志页）
      state    → 合并任务列表
  动作（同步/暂停/删除/保存/清空/导出）→ POST/PATCH/DELETE → 乐观更新 + 失败回滚 + Toast
  重连：指数退避 1s→8s，页面重新可见时立即重连
```

### 4.4 设计令牌（与 `ui_colors` 一一对应）

| 变量 | 值 | 用途 |
|---|---|---|
| `--ws-bg` | `#0E0F10` | 内容区底 |
| `--ws-sidebar` | `#161719` | 侧栏 |
| `--ws-card` | `#1C1D1F` | 卡片 |
| `--ws-card-hover` | `#232427` | hover |
| `--ws-border` | `#2A2C2E` | 分隔线 / 边框 |
| `--ws-text` | `#EDEDEE` | 主文字 |
| `--ws-text-2` | `#A0A2A6` | 次要 |
| `--ws-text-3` | `#6E7176` | 弱化 |
| `--ws-primary` | `#2B6CF6` | 主按钮 / 选中 |
| `--ws-primary-dim` | `#1E355C` | 选中底 |
| `--ws-success` | `#4CC38A` | 成功 |
| `--ws-danger` | `#F0554D` | 失败 |
| `--ws-warn` | `#E6A23C` | 同步中 |

字号与圆角：标题 18/15px，正文 12/13px，弱化 11px；卡片圆角 10px，按钮 6px，侧栏项 6px。间距序列 2/4/6/8/10/12/14/16/18。

### 4.5 交互细节（容易漏的）

- 「立即同步」在 `running` 或 `issues` 非空时禁用；禁用原因用 hover title + 状态区红字双写。
- 密钥字段 `type=password`，保存后显示「已设置」占位，不回显原文；`has_token=false` 时提示「未填写」。
- 换目录保存：前端先确认（「会清空共识快照，下次全量重建」），后端返回 `root-changed` 再带 `force:true` 重发。
- 删除任务二次确认，文案说明「状态库保留在 tasks/<id>.db」。
- 向导第三步实时跑 `issues` 校验预览（`根 ⇄ 主机 · 房间`），不合法逐条红字。
- 日志页空态「暂无记录」，未选任务时默认当前任务，下拉含「全部任务」。
- SSE 断开时顶部黄条「实时连接中断，正在重连」，不阻塞操作。

### 4.6 与 exe 的差异（写进 README）

托盘菜单、`浏览…` 原生对话框、单窗口标题栏、系统图标、离线时的「等待对端上线」超时提示条（改为页内状态）。

## 5. 构建与安装

没有构建步骤：宿主半是纯 ESM（`lib/index.js` 直接 import），客户端半是手写 bundle（`lib/client.js`），
`dsh` 直接从 `lib/*.js` 加载。这比原计划的 `tsc + esbuild` 少一条链，代价是客户端不能用 JSX
（用 `React.createElement` 的 `h()` 别名写）。

```bash
# 1. Rust 守护进程
cd sync
cargo build --release -p sync-daemon      # → target/release/sync-daemon.exe
cargo test -p sync-daemon                 # 16 项单元测试

# 2. 把守护进程放到插件自己的目录（与 exe 的 Programs\WinSync 分开）
mkdir -p "%LOCALAPPDATA%\dsh-winsync\bin"
copy target\release\sync-daemon.exe "%LOCALAPPDATA%\dsh-winsync\bin\winsync-daemon.exe"

# 3. 装进 profile
dsh plugin --profile desktop add file:C:\Users\gjn86\WorkBuddy\spaces\2026_11_19_20-38-45\dsh-winsync
```

**改过插件文件后必须重装一次**：`nodeLinker: hoisted` 把包**硬链**进 `profiles/<name>/node_modules`，
新增文件不会自动出现（P3 期间就踩过：profile 里只有 3 个旧文件，缺 `daemon.js`，插件直接加载失败）。
重装的做法是先删后装：

```bash
rm -rf ~/.dsh/profiles/desktop/node_modules/dsh-winsync
dsh plugin --profile desktop install --force
```

守护进程的查找顺序（`lib/daemon.js#resolveBinary`）：
偏好 `binPath` → `WINSYNC_DAEMON_BIN` → `<状态目录>/bin/winsync-daemon.exe`
→ `%LOCALAPPDATA%\Programs\WinSync\winsync-daemon.exe` → `PATH` → 仓库产物（开发用）。

客户端 bundle 的包装方式（`lib/client.js` 末尾）：

```js
window.__ModuleLoader__.load({ id: 'dsh-winsync', factory: function (require) {
  // …整个组件树…
  return { name: 'dsh-winsync', inject: ['slots'], apply(ctx) { /* main + sidebar.panellist */ } }
}})
```

CSS 作为字符串内联（`const CSS = \`…\`` + `ensureStyles()` 注入 `<style data-winsync>`），
不产生额外资源文件与路由。

安装后重启 dsh，左侧栏应出现「同步」图标；控制台不得出现 `duplicate prefix route` 或 slot 重复注册。

## 6. 里程碑与任务分解

### P0 冒烟（最高未知项，先做）
| # | 任务 | 完成标志 |
|---|---|---|
| 0.1 | `dsh-winsync` 骨架（package.json / patch / icon / tsconfig） | `dsh plugin add` 成功，bundles 出现该包 |
| 0.2 | 宿主 `index.ts` 注册 `/winsync/api/ping` | curl 返回 `{ok:true}` |
| 0.3 | 客户端注册 `main` 面板 + `sidebar.panellist` 图标行 | dsh 重启后左侧栏出现「同步」，点击后主区显示面板（首页即可，无需开会话） |
| 0.4 | 卸载验证 | 移除插件后标签消失、无残留样式/路由 |

> 0.3 不过就地停：说明 ModuleLoader 包装或 slot props 不对，改到能跑再往下。

### P1 daemon 引擎
| # | 任务 | 完成标志 |
|---|---|---|
| 1.1 ✅ | crate 骨架、自有互斥体、`daemon.json`、`/health` | 双击运行后文件生成，重复启动被拒；exe 同时在跑也不受影响 |
| 1.2 ✅ | `/api/state` 只读快照 | curl 能读到现有任务与设置 |
| 1.3 ✅ | 任务 CRUD（含换目录 force、校验） | 与 exe 生成的 `config.json` 结构一致 |
| 1.4 ✅ | 调度 + watcher + `session::run` 接入 | 触发同步后 events 流有 log/progress/done |
| 1.5 ✅ | 活动查询、CSV 导出、日志环形/落盘、日志清理 | 导出文件与 exe 导出逐行一致 |
| 1.6 ✅ | 限速、自启、打开目录、`/fs/list` | 限速生效可用 `--wait` 场景验证；路径穿越被拒 |
| 1.7 ✅ | 单测 | 调度抖动、`issues` 拦截、root-changed、鉴权失败 4 组用例 |

### P1 完成记录（2026-11-21）

代码：`sync/crates/sync-daemon/`（约 1,900 行含测试与脚本），文档见该目录 `README.md`。

| 验收项 | 证据 |
|---|---|
| 启动与握手 | 启动后 `daemon.json` 含 `pid/port/token/version/started_at`；`/health` 免鉴权、无 token 泄露；重复启动同目录会打印 already-running 并 exit 0 |
| 状态目录隔离 | `WINSYNC_STATE_DIR` 生效，`/api/state.data_dir` 指向插件自己的树；`Config::dir()` 仅加一个分支，exe 不设该变量行为不变 |
| 鉴权 | 无 Bearer → 401；token 定长比较（单测覆盖） |
| 任务 CRUD | 建/改/删全通；换目录不带 `force` → `409 root-changed`；坏配置（`http://`、空密钥）→ `400 bad-request` |
| 调度 + 真同步 | `e2e-sync.mjs`：真 relay + 真 daemon + 真 CLI，双向各落地 3 个文件，SSE 收到 7 条 log 与 `done{report}`，`runtime.last` 正确收尾 |
| 记录与导出 | 同步后 `/api/activities` 有 4 条；`/api/export.csv` 表头与 exe 一致 |
| 日志 | 内存环形 400 行 + `sync.log` 落盘；`/api/logs`、`/api/logfile` 可读 |
| 目录重叠 | 5 组单测（同目录大小写/斜杠、父子互含、兄弟不重叠、盘根覆盖、空路径）；命中返回 `root-conflict` 并点名 exe 任务 |
| 路径安全 | `/api/fs/list` 拒绝 `..`、绝对路径、盘符；`/api/open-dir` 只允许任务根目录 |
| 单测 | 15 项全绿；`scripts/smoke.mjs` 11 项 HTTP 契约全绿 |

已知取舍（写在这里避免以后翻旧账）：

- `auto`（自动同步开关）不落盘，重启回到开——与 exe 行为一致。
- 硬杀进程不会执行 Drop，`daemon.json` 会残留；插件侧按 pid 判活回收，daemon 启动时也会覆盖写。
- 互斥体名带状态目录哈希：同一棵树仍单实例，不同树（测试/多实例）互不阻塞。

### P2 宿主与工具
| # | 任务 | 完成标志 |
|---|---|---|
| 2.1 ✅ | 代理 + SSE 直通 + 错误映射 | 浏览器只访问 dsh 同源，daemon token 不出现在网络面板 |
| 2.2 ✅ | daemon 自动拉起/复用/退出策略 | 杀进程后 UI 一键恢复 |
| 2.3 ✅ | 4 个工具 | 在 dsh 里问「同步状态」能拿到真实数据 |
| 2.4 ✅ | 工具安全 | 返回中无 token；`sync_now` 缺任务时自动选中唯一任务并说明 |

### P2 完成记录（2026-11-21）

宿主半拆成四个纯 ESM 模块（仍不引入构建步骤，`dsh` 直接加载 `lib/*.js`）：

| 文件 | 职责 |
|---|---|
| `lib/daemon.js` | 二进制发现（偏好 → 环境变量 → 安装目录 → PATH → 仓库产物）、握手复用/回收、拉起等待、`/api/shutdown` 优雅退出 |
| `lib/proxy.js` | `/winsync/api/*` 转发：注入 Bearer 与 `X-WinSync-Client`、方法白名单、256 KB 上限、SSE 逐块直通、5xx → 502 `daemon-unavailable` |
| `lib/tools.js` | `winsync_tasks` / `winsync_status` / `winsync_sync_now` / `winsync_recent_activity` |
| `lib/prefs.js` | `<state>\plugin.json`：`binPath` / `stopDaemonOnExit` / `autoSpawn` |

与计划的偏离（都是刻意的）：

- **不建 `src/*.ts` + esbuild**：宿主半只有几百行、没有 JSX，直接手写 ESM 省掉一条构建链；客户端半仍是手写 bundle（见 4.1）。
- **偏好文件放在自己的状态树**（`%LOCALAPPDATA%\dsh-winsync\plugin.json`），不是计划里的 `%APPDATA%\WinSync\dsh-plugin.json`——按「与 exe 互不影响」的硬约束，连一个偏好文件也不共享。
- **daemon 新增 `POST /api/shutdown`**：插件卸载时先请求优雅退出，避免只能 TerminateProcess 而留下 `daemon.json`。被硬杀时仍由插件的陈旧握手回收兜底。
- **工具用标准 JSON Schema 直接注册**，不经 `defineTool`：后者对 `parameters` 做 DSL 转换，需要 `@deepseek-ai/dsh-tools` 的裸导入，我们的包不参与打包，避免解析风险。

验收：

- `node test/p2-smoke.mjs`（11 项）：注册 1 条 prefix 路由 + 4 个工具；`/ping` 不泄露 token；`/diag` 真拉起 daemon
  且只回 token 指纹；经代理建任务、409 `root-changed` 原样透传、404 透传；SSE 首帧 `hello`；
  3 个只读工具返回契约正确；卸载后 daemon 停止且 `daemon.json` 被清掉。
- `node test/client-smoke.mjs`：客户端注册契约（main 面板 + sidebar 行）仍通过。

P3 期间对宿主半的两处增补：

- **`/diag` 不再吐 token 明文**：握手文件里的 token 是 daemon 的唯一凭证，`/diag` 只返回
  `{present, length, fingerprint}`（`lib/proxy.js#redactHandshake`）。这条路由不走 dsh 的会话闸门
  （插件 prefix 路由是裸的），所以不能在响应里放凭证。
- **删掉 P0 的 `/probe/*` 过渡别名**：正式界面只用 `/winsync/api/*`，留着就是多余的本机写入口。

### P3 界面复刻
| # | 任务 | 完成标志 |
|---|---|---|
| 3.1 ✅ | 设计令牌 + 侧栏 + 页面骨架 | 三页可切换，180px 侧栏，色值与表一致 |
| 3.2 ✅ | 任务卡 + 进度 + 状态 + ⋯菜单 + 暂停 | 三档宽度（≥1000 / 800 / 640）不塌 |
| 3.3 ✅ | 创建向导三步 | 校验、沿用连接、返回/取消行为与 exe 一致 |
| 3.4 ✅ | 任务设置弹窗 | 保存/暂停/删除全通，密钥不回显 |
| 3.5 ✅ | 日志页表格 | 筛选/搜索/斑马纹/导出/清空 |
| 3.6 ✅ | 设置页 + 运行日志浮层 | 与 exe 条目一一对应（含禁用态说明） |
| 3.7 ✅ | 空态/错误态/断线态 | 每页各有明确引导文案，无白屏 |

### P3 完成记录（2026-11-21）

`lib/client.js` 约 1,100 行手写 bundle（v0.3.0），结构：

| 部分 | 内容 |
|---|---|
| `CSS` + `ensureStyles()` | `ws-` 前缀全量样式，色值照 `ui_colors`；只注入一次（`<style data-winsync>`） |
| `useStore()` | `/state` 拉快照 + `EventSource('/winsync/api/events')`；处理 `hello/state/log/progress/done/lagged`；断线指数退避 400ms→8s；统一 `run(work, successText)` 做 toast + 刷新 |
| `WinsyncApp` | 180px 侧栏（品牌 + 三页导航 + 底部「+ 创建同步任务」）+ 主区；断线横幅、同步服务不可用横幅、toast、两个弹窗、原始日志浮层都在这一层 |
| `TasksPage` / `TaskCard` | 进度条、三列信息（路径 / 房间 / 规则）、「立即同步」禁用条件、`⋯` 菜单（设置/打开目录/查看日志/删除）、暂停圆钮 |
| `LogsPage` | 任务筛选 + 状态筛选 + 搜索 + 表格 + 导出 CSV（`window.open` 直落 daemon 的 `/export.csv`）+ 清空 |
| `SettingsPage` | 自启开关、上下行限速、记录保留（天数/条数）、立即清理、诊断、保存 |
| `Wizard` | 三步（服务器 → 本机目录 → 确认）；`issues` 实时预检，有问题时「下一步/创建」禁用并把原因挂在 `title` 上 |
| `TaskEditor` | 改目录要二次确认，确认后带 `force: true`（daemon 那边就是 409 `root-changed`） |
| `RawLog` | 浮层贴底滚动，读 `runtime.logs` 环形缓冲 |

浏览器的目录选择器拿不到系统路径（`<input type="file" webkitdirectory>` 只给相对路径），
所以第二步是**粘贴绝对路径**，并在 hint 里写明原因——这点与 exe 不同，已记进 README。

响应式用**容器查询**而不是媒体查询：面板宽度由 dsh 的分栏决定，跟视口没关系。

| 面板宽度 | 表现 |
|---|---|
| > 820px | 三列信息 |
| ≤ 820px | 两列 |
| ≤ 640px | 单列，侧栏收窄到 132px，品牌名只留图标 |

验收（`node test/cdp-p3.mjs "<url>" <port>`，CDP 真实鼠标事件驱动，无 npm 依赖）：

- 空态 → 向导 1/3 → 2/3 → 3/3 → 任务卡 → 日志页 → 设置页 → 600px 窄态，逐张截图在 `test/shots/`；
- 全程 `console` 零报错；
- 容器查询实测：900px → 3 列（`196.3px × 3`）、780px → 2 列（`240.5px × 2`）、600px → 1 列（`389px`）且
  `.ws-brand-name` 隐藏；
- 建出的任务在无中继时会走真实失败路径，界面显示
  `连接服务器失败: IO error: 由于目标计算机积极拒绝，无法连接。 (os error 10061)`
  （daemon 侧新加 `util::describe_error` 去掉 anyhow 链里重复的段，此前同一句会打印两遍）。

同时验证的隔离行为：

- 插件在独立状态目录下自建 `<state>/bin/winsync-daemon.exe` 并成功拉起，未碰 exe 的 `%APPDATA%\WinSync`；
- 宿主被硬杀后 `daemon.json` 残留 → 下次 `ensure()` 按 pid 判活回收，日志实证
  `[ensure] 丢弃陈旧握手（pid=45412 已退出）`。

### P3 界面遮挡修复（2026-11-22）

桌面端截图暴露出一个比配色严重得多的问题：**面板整块盖住了 dsh 自己的界面**。左侧 dsh 侧栏、
Windows 标题栏（「应用 / 编辑」菜单、最小化/关闭按钮）全被压住，进去以后没有出口。

根因在 `.ws-root` 的定位方式。原样式是 `position:absolute; inset:0`，而宿主 `.mainPanel` 是
`position:static`，于是包含块一路回溯到 `.frame`（`position:relative`）：

- 横向：铺满整个 frame，连 280px 的 dsh 侧栏一起盖掉；
- 纵向：`.frame` 在 Windows 上只靠 `padding-top: var(--dsh-windows-titlebar-height)` 让出标题栏，
  而绝对定位元素贴的是 **padding box** 的内边缘——`top:0` 正好落在标题栏里。

修法是放弃绝对定位，按宿主给的盒子排版：

| 改动 | 说明 |
|---|---|
| `.ws-root` 改 `position:relative; flex:1 1 auto; min-height:0; height:100%` | 宿主 `main` 槽外面是 `display:contents` 的包装，面板直接成为 `.mainPanel`（flex column）的子项，自动落在标题栏之下、侧栏之右 |
| 面板内 `← 返回 dsh` | `inject` 增加 `layout`，调 `ctx.layout.selectPanel(null)` 把主区交还给会话；宿主没有该服务时按钮不渲染 |
| `client-smoke` 补两条断言 | `inject` 必须含 `layout`；面板壳必须暴露 `onBack`，且调用后 `selectPanel(null)` |

实测（1440×960 的真实页面）：面板矩形从 `0,0,1406×863` 变成 `280,0,1126×863`——正好是
`.mainPanel` 的盒子；点「返回」后 `.ws-root` 消失、会话输入框回来。

> 教训：插件挂在 `main` 槽时**不要用 `position:absolute; inset:0` 撑满**。宿主只保证槽内容落在
> 主区那一格里，绝对定位会被最近的 positioned 祖先（`.frame`）接管，把别人的界面吃掉。

### P3 界面细节调整（2026-11-22，提交 `a2c11a7`）

P3 交付时的界面只在空态和单任务上验证过。这次灌了 5 个真实任务（正常 / 仅上传暂停 /
仅下载暂停 / 超长名 / 服务器不可达）逐屏看过之后，改掉这些：

| 问题 | 现象 | 处理 |
|---|---|---|
| 状态自相矛盾 | 上轮失败、这轮还在等对端时，卡头写「同步中…」、下面挂红字错误 | 卡头改「重试中…」；错误行只在 running/paused 时加「上次失败：」前缀 |
| 页头信息错位 | `GRAY_GUO \| 127.0.0.1:18443 · 房间 office` 只取了第一个任务的房间 | 改成「设备 · N 个任务」，房间留给每张卡 |
| 统计噪音 | 无变化时也念「推送 0 拉取 0 删除 0 冲突 0」 | 全 0 收成「无变更」，有值只列非零项 |
| 弹窗被切 | 编辑弹窗最后两个输入框被底栏切掉 | flex 子项补 `min-height:0`（不设它 `overflow:auto` 不生效）+ 字段间距 12→10px、输入框统一 32px |
| 滚动条不可见 | 弹窗/列表能滚但没提示 | `::-webkit-scrollbar` 显式给 8px 轨道与滑块 |
| 文字过暗 | 三列标签、次要说明用 `#6E7176`，几乎看不清 | 提到 `#8B8E94`；日志表头提亮并加字重、行分隔加深 + hover |
| 图标像方块 | 16px 文件夹挤在 30px 深色块里 | 18px，底色 `#26282B`、前景 `#B9BBBF` |
| 主次不分 | 暂停卡的手动同步仍是主色蓝 | 暂停态换次级按钮；「自动同步」复选框改成开关 |
| 向导步骤条 | 三步全是同一个蓝色，看不出在哪一步 | 分「已完成（暗蓝+灰）/ 当前（亮蓝+白粗）」，预览路径与目标分两行 |

走查工具（都在 `test/`，不进发布包）：

| 脚本 | 用途 |
|---|---|
| `seed-demo.mjs <stateDir> [--reset] [--sync]` | 按场景灌任务；`--reset` 先清空，可反复跑 |
| `peer-loop.mjs <workspace> <root> [minutes]` | 常驻 CLI 对端，让自动同步真跑通并产生传输数字 |
| `cdp-view.mjs <url> <port> <step>` | **一次只拍一屏**（step: tasks/menu/editor/raw/logs/settings/narrow/wizard/wizard3） |
| `cdp-eval.mjs <url> <port> "<js>"` | 在真实页面跑表达式，区分「没数据」和「选择器不对」 |

踩到的坑（都会反复踩）：

1. **`Page.captureScreenshot` 会让 dsh 重排面板**，任务列表被清空。连拍多张时，第二张起全在空态上跑，
   表现为「脚本找不到卡片按钮」。所以 `cdp-view` 一次只做一步、拍一张就退出。
2. **dsh 认证后会 `location` 换掉 `?token=`**，中途页面重挂一次；不等 URL 干净就操作，会撞在空态上。
3. **`document.querySelectorAll('.ws-card')` 会命中 dsh 自己的元素**，必须带 `.ws-root` 前缀。
4. 侧栏也有「设置」，点菜单项时必须限定在 `.ws-menu` 里，否则点成导航。

验收：`client-smoke`、`p2-smoke` 全过；任务/日志/设置/编辑弹窗/向导/日志浮层逐屏截图并逐句读文案，
console 零报错。

### P4 联调与交付
| # | 任务 | 完成标志 |
|---|---|---|
| 4.1 | 两端实测：双向 / 仅上传 / 仅下载 | 文件与删除行为符合 README 的同步行为表 |
| 4.2 | 安全阀：过滤不一致、删除熔断、目录不一致 | 三种情况都中止并给出可读原因 |
| 4.3 | 隔离与共存：exe 与 daemon 同时运行 | 两边任务各自独立、配置互不写；同目录任务被 `root-conflict` 拦下 |
| 4.4 | 回归：`cargo test --workspace` | 全绿（现有 18 项 + 新增 4 项） |
| 4.5 | 文档：README + 差异清单 + 排障 | 覆盖「标签不见了 / 服务未启动 / 找不到 exe」三类问题 |

## 7. 测试矩阵

| 场景 | 手段 | 期望 |
|---|---|---|
| 首次启动无配置 | 清空 config.json | 空态引导，不报错 |
| 旧版扁平配置 | 放入 v0.2 格式 config | 自动迁移为一个任务，基线保留 |
| 任务换目录 | PATCH 不带 force → 带 force | 先 409，再成功且 base 清空 |
| 过滤不一致 | 两端不同规则 | 会话中止，日志写明双方签名 |
| 删除熔断 | 对端清空 30+ 文件 | 中止并列出路径 |
| 限速 | 64 KB/s 传 2 MB | 实际耗时 ≥ 30s |
| SSE 断线 | 杀 daemon → 重启 | 前端 8s 内恢复，期间不丢操作（按钮置灰 + Toast） |
| 大目录 | 3000 文件 | 打开日志页 < 200ms，扫描复用缓存 |
| 并发运行 | exe 在跑时启 daemon | 两者同时存活，各写各的 config/DB，互不报错 |
| 目录重叠 | 插件任务 root 与 exe 任务 root 相同或互为父子 | 拒绝启动，`root-conflict` 点名占用任务，文件零改动 |
| 心跳隔离 | 退出 exe | daemon 与页面无感知，同步继续 |
| 鉴权 | 无 Bearer 请求 `/api/state` | 401，且不泄露任何任务信息 |

## 8. 风险与回滚

| 风险 | 触发信号 | 回滚动作 |
|---|---|---|
| slot 或 ModuleLoader 不兼容 | P0 加载失败 | 只删 `dsh-winsync` 目录与 bundles 条目，dsh 恢复原状（不动 profile 其他插件） |
| daemon 与 exe 抢锁 | 启动即退 2 | 属预期保护；文档引导「关一个」 |
| 路由与现有插件冲突 | 启动报 duplicate prefix | 换前缀 `/winsync-v1/*`（已预留版本号） |
| 写入损坏配置 | JSON 解析失败 | daemon 启动时 `config.json` 先备份 `.bak-<date>` 再写；复用 exe 的原子写 |
| 浏览器缓存旧 client 包 | 改了 UI 不生效 | 构建产物文件名带 hash，patch 里指向 `lib/client.<hash>.js`；README 写清重启步骤 |

## 9. 首日必须跑通的三个点

1. `dsh plugin add file:` 能装上，重启后左侧栏 `sidebar.panellist` 出现「同步」图标行，点击切到主区面板。
2. `ctx.webServer.register` 的 `/winsync/api/ping` 能被页面 fetch 到（同源、带会话）。
3. `winsync-daemon.exe` 在 dsh 插件里被 spawn 起来，`/health` 通，退出后 `daemon.json` 清理干净。

三点任一不通过，都先停下来解决，不进 P1。

---

确认这份计划后，我按 P0 → P1 → P2 → P3 → P4 执行；每期结束给出可验证的结果（curl 输出、截图或测试记录），不会一口气做完再给你看。
