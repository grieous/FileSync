# dsh-winsync

把 WinSync 做成 DeepSeek Harness 的插件：左侧栏一个「同步」入口，主区面板复刻 exe 的界面；
同步本身由 `winsync-daemon.exe`（Rust，复用 `session::run`）执行，插件只做代理与界面。

```
浏览器 ──同源 /winsync/api──▶ 插件宿主(Node) ──注入 Bearer──▶ winsync-daemon ──▶ relay / 对端
                                                        └─ %LOCALAPPDATA%\dsh-winsync
```

## 与 WinSync.exe 的关系

**互不影响**（硬约束）：不共享配置、状态库、日志、互斥体，也不互相启停。
插件自己的那一棵树在 `%LOCALAPPDATA%\dsh-winsync`，exe 仍在 `%APPDATA%\WinSync`。

唯一必须协同的地方：**同一个目录不能两边同时同步**（否则双方都会看到「对端删除」而互删）。
daemon 启动任务前只读地看一眼 exe 的配置，命中就拒绝，返回 `root-conflict` 并点名占用任务。

## 安装

```bash
# 1. 守护进程（release 更划算：同步循环是 CPU/IO 密型）
cd ../sync && cargo build --release -p sync-daemon
mkdir -p "%LOCALAPPDATA%\dsh-winsync\bin"
copy target\release\sync-daemon.exe "%LOCALAPPDATA%\dsh-winsync\bin\winsync-daemon.exe"

# 2. 插件
dsh plugin --profile desktop add file:C:\path\to\dsh-winsync
```

**改完插件代码要重装一次**：profile 用 `nodeLinker: hoisted`，包是**硬链**进去的，新增文件不会自动出现。

```bash
rm -rf ~/.dsh/profiles/desktop/node_modules/dsh-winsync
dsh plugin --profile desktop install --force
```

然后重启 dsh，左侧栏出现「同步」。

## daemon 从哪来

按顺序查找：

1. `<state>\plugin.json` 的 `binPath`
2. 环境变量 `WINSYNC_DAEMON_BIN`
3. `<state>\bin\winsync-daemon.exe`（推荐，插件自有目录）
4. `%LOCALAPPDATA%\Programs\WinSync\winsync-daemon.exe`
5. `PATH` 里的 `winsync-daemon.exe`
6. 仓库开发产物 `sync/target/debug|release/sync-daemon.exe`

都没有时接口返回 `daemon-missing`，界面会给出提示。

## 界面

三页 + 两个弹窗 + 一个浮层，全部在 `lib/client.js` 里手写（无构建步骤、无 JSX）：

| 页面 | 内容 |
|---|---|
| 同步任务 | 任务卡：进度条、路径/房间/规则三列、「立即同步」、暂停圆钮、`⋯`（设置/打开目录/查看日志/删除） |
| 同步日志 | 任务与状态筛选、搜索、表格、导出 CSV、清空 |
| 设置 | 开机自启、上下行限速、记录保留（天数/条数）、立即清理、诊断、保存 |
| 弹窗 | 创建向导（三步 + 预检）、任务设置（改目录要二次确认） |
| 浮层 | 原始日志（最近 400 行，贴底滚动） |

响应式用容器查询，因为面板宽度取决于 dsh 的分栏而不是视口：> 820px 三列，≤ 820px 两列，
≤ 640px 单列且侧栏收窄到 132px。

面板只占 dsh 主区那一格：dsh 的左侧栏、Windows 标题栏和菜单始终露在外面；面板左下角的
**`← 返回 dsh`** 调 `ctx.layout.selectPanel(null)`，把主区交还给当前会话（点 dsh 侧栏里的会话也一样）。

**与 exe 的差异**：浏览器拿不到系统的目录选择框（`<input type="file" webkitdirectory>` 只给相对路径），
所以创建向导第二步是**粘贴绝对路径**，hint 里写明了原因。

## 偏好

`%LOCALAPPDATA%\dsh-winsync\plugin.json`（放在自己的树里，不碰 exe）：

```json
{ "binPath": "", "stopDaemonOnExit": true, "autoSpawn": true }
```

- `stopDaemonOnExit`：默认退出即停。想 7×24 常驻就设 false，并同时打开 daemon 的开机自启。
- 复用的 daemon（不是本插件拉起的）永远不会被停。
- 宿主被硬杀时 `daemon.json` 会残留，下次启动按 pid 判活回收，不会卡住。

## 接口

浏览器只访问 `/winsync/api/*`（dsh 同源）：

| 路径 | 说明 |
|---|---|
| `GET /ping` | 插件自检：状态目录、daemon 快照；不含 token |
| `GET /diag` | 拉起 daemon 并回 `health`；握手里的 token 只给指纹与长度 |
| `* /tasks` `* /state` `* /events` … | 原样代理到 daemon，Bearer 在服务端注入 |

方法白名单 `GET/POST/PATCH/DELETE`，请求体上限 256 KB，上游 5xx 统一翻成
`502 {code:'daemon-unavailable'}`。

> 插件 prefix 路由不走 dsh 的会话闸门，所以这两条自检接口刻意不回任何凭证——
> 只监听 `127.0.0.1`，且响应里没有可读的密钥。

## agent 工具

| 工具 | 副作用 | 说明 |
|---|---|---|
| `winsync_tasks` | 无 | 任务清单（密钥只给 `has_token`） |
| `winsync_status` | 无 | 进度、上次结果、失败原因、下次自动同步倒计时 |
| `winsync_sync_now` | **有**（可能传播删除） | 默认只返回「已开始」；`wait=true` 等到结束 |
| `winsync_recent_activity` | 无 | 最近的文件级记录 |

## 排障

| 现象 | 先查什么 | 处理 |
|---|---|---|
| 左侧栏没有「同步」 | `dsh plugin --profile <p> list` 里有没有 `dsh-winsync`；浏览器 console 有没有模块加载错误 | 少东西就重装：删 `~/.dsh/profiles/<p>/node_modules/dsh-winsync` 再 `dsh plugin --profile <p> install --force` |
| 面板打开是「同步服务不可用」 | 面板「设置 → 诊断」里的 `daemon` 段；`%LOCALAPPDATA%\dsh-winsync\daemon.json` 是否存在 | 点「重试」；仍失败就看 daemon 日志（`<state>\logs\`） |
| 提示 `daemon-missing` | `<state>\bin\winsync-daemon.exe`、`Program Files\WinSync\`、`PATH` | `cargo build -p sync-daemon --release` 后把 exe 放进 `<state>\bin\`，或在设置里填绝对路径 |
| 改了界面却不生效 | 是不是只 `pnpm install` 没重启 dsh | 客户端 bundle 在页面加载时注入，**必须重启 dsh**；重装前记得删旧的 `node_modules/dsh-winsync`（pnpm 会硬链旧快照） |
| 进了同步页退不出来 | 左下角 `← 返回 dsh`；或直接点 dsh 侧栏里的会话 | 两者都调 `layout.selectPanel(null)`；宿主版本太旧没有 `layout` 服务时按钮不渲染 |
| 同步「看着很慢」 | 设置里的限速是不是非 0 | 1.5 MB @ 256 KB/s 实测 6.6 秒。旧版本把 KB/s 当 kbit/s，慢 8 倍，见 `sync/README.md` |
| 报 `Connection reset without closing handshake` | 房间是不是已经满了（一个房间最多 2 端） | 中继发完 `Bye` 立刻断 socket，客户端只看得到协议错误。先确认没有第二个进程/任务连同一个房间；对端残留的旧会话要重启 exe 才会断 |

## 测试

```bash
node test/client-smoke.mjs   # 客户端注册契约（main 面板 + 左栏行 + 返回对话出口）
node test/p2-smoke.mjs       # 宿主：路由、代理、CRUD 透传、SSE、工具、卸载清理、/diag 脱敏
node test/cdp-inspect.mjs <url> [port] [--aria=同步]   # 真实页面里的渲染与截图

# 同步本体（双端、安全阀、限速、与 exe 的隔离）在仓库根的 sync/tools/
node ../sync/tools/e2e.mjs   # 77 项断言：真 relay + 两个真对端，约 25 秒

# 分步视觉走查（需要先起一个实例）：空态→向导三步→任务卡→日志→设置→窄态
CDP_SIZE=1440,960 node test/cdp-p3.mjs "<url>" [port]   # 截图落在 test/shots/
```

CDP 脚本一律用真实鼠标事件（`Input.dispatchMouseEvent`）而不是 DOM 的 `.click()`：
面板里「+ 创建同步任务」这类文字在父 `div` 上也有，`.click()` 会点到没有 onClick 的那一层。
