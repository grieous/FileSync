# dsh-winsync

把 WinSync 做成 DeepSeek Harness 的插件：左侧栏一个「同步」入口，主区面板复刻 exe 的界面；
同步本身由 `sync-daemon.exe`（Rust，复用 `session::run`）执行，插件只做代理与界面。

```
浏览器 ──同源 /winsync/api──▶ 插件宿主(Node) ──注入 Bearer──▶ sync-daemon ──▶ relay / 对端
                                                        └─ %LOCALAPPDATA%\dsh-winsync
```

## 与 WinSync.exe 的关系

**互不影响**（硬约束）：不共享配置、状态库、日志、互斥体，也不互相启停。
插件自己的那一棵树在 `%LOCALAPPDATA%\dsh-winsync`，exe 仍在 `%APPDATA%\WinSync`。

唯一必须协同的地方：**同一个目录不能两边同时同步**（否则双方都会看到「对端删除」而互删）。
daemon 启动任务前只读地看一眼 exe 的配置，命中就拒绝，返回 `root-conflict` 并点名占用任务。

## 安装

```bash
dsh plugin --profile desktop add file:C:\path\to\dsh-winsync
```

改完代码不用重装（profile 里是链接），重启 dsh 生效。

## daemon 从哪来

按顺序查找：

1. `<state>\plugin.json` 的 `binPath`
2. 环境变量 `WINSYNC_DAEMON_BIN`
3. `%LOCALAPPDATA%\Programs\WinSync\winsync-daemon.exe`
4. `PATH` 里的 `winsync-daemon.exe`
5. 仓库开发产物 `sync/target/debug|release/sync-daemon.exe`

都没有时接口返回 `daemon-missing`，界面会给出提示。构建：`cargo build -p sync-daemon`。

## 偏好

`%LOCALAPPDATA%\dsh-winsync\plugin.json`（放在自己的树里，不碰 exe）：

```json
{ "binPath": "", "stopDaemonOnExit": true, "autoSpawn": true }
```

- `stopDaemonOnExit`：默认退出即停。想 7×24 常驻就设 false，并同时打开 daemon 的开机自启。
- 复用的 daemon（不是本插件拉起的）永远不会被停。

## 接口

浏览器只访问 `/winsync/api/*`（dsh 同源）：

| 路径 | 说明 |
|---|---|
| `GET /ping` | 插件自检：状态目录、daemon 快照；不含 token |
| `GET /diag` | 拉起 daemon 并回 `health`，排查「界面连不上」用 |
| `* /tasks` `* /state` `* /events` … | 原样代理到 daemon，Bearer 在服务端注入 |

方法白名单 `GET/POST/PATCH/DELETE`，请求体上限 256 KB，上游 5xx 统一翻成
`502 {code:'daemon-unavailable'}`。

## agent 工具

| 工具 | 副作用 | 说明 |
|---|---|---|
| `winsync_tasks` | 无 | 任务清单（密钥只给 `has_token`） |
| `winsync_status` | 无 | 进度、上次结果、失败原因、下次自动同步倒计时 |
| `winsync_sync_now` | **有**（可能传播删除） | 默认只返回「已开始」；`wait=true` 等到结束 |
| `winsync_recent_activity` | 无 | 最近的文件级记录 |

## 测试

```bash
node test/client-smoke.mjs   # 客户端注册契约（main 面板 + 左栏行）
node test/p2-smoke.mjs       # 宿主：路由、代理、CRUD 透传、SSE、工具、卸载清理
node test/cdp-inspect.mjs <url> [port] [--aria=同步]   # 真实页面里的渲染与截图
```
