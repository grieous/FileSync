# sync-daemon

WinSync 的无界面后端，给 DeepSeek Harness 插件用：HTTP + SSE，复用 `sync-client` 的全部同步算法。

```
dsh 插件（Node）  ──HTTP/SSE──▶  sync-daemon  ──session::run──▶  relay / 对端
  浏览器只认插件                 只监听 127.0.0.1:0
```

## 与 WinSync.exe 的隔离（硬约束）

| 资源 | exe | daemon |
|---|---|---|
| 配置 / 状态库 / 日志 | `%APPDATA%\WinSync` | `%LOCALAPPDATA%\dsh-winsync` |
| 单实例锁 | `Local\WinSync_SingleInstance_v1` | `Local\dsh-winsync-daemon-v1-<state-dir hash>` |
| 进程 | 不被 daemon 启停 | 不被 exe 启停 |

两者可以同时运行，各写各的。唯一交集是**同一个目录不能两边同时同步**：
启动前只读地看一眼 exe 的 `config.json`，目录相同或互为父子就拒绝，返回 `root-conflict`，
文案点名是 exe 的哪个任务占用了该目录。

实现上只有一处对既有代码的增量改动：`Config::dir()` 支持 `WINSYNC_STATE_DIR`。
exe 不设这个变量，行为零变化。

## 构建与运行

```bash
cargo build -p sync-daemon
./target/debug/sync-daemon.exe            # 状态目录默认 %LOCALAPPDATA%\dsh-winsync
WINSYNC_STATE_DIR=D:\tmp\wsd ./target/debug/sync-daemon.exe   # 换一棵树（测试用）
```

启动后在状态目录写 `daemon.json`（`pid/port/token/version/started_at`），退出时删除。
硬杀时文件会留下，插件侧会按 pid 判活回收。

环境变量：

| 变量 | 作用 |
|---|---|
| `WINSYNC_STATE_DIR` | 状态根目录；不设则 `%LOCALAPPDATA%\dsh-winsync` |
| `WINSYNC_LOG` | 日志级别，默认 `info`（如 `debug`） |

## 接口

除 `/health` 外全部要求 `Authorization: Bearer <daemon.json 里的 token>`。
错误统一 `{ok:false,error:{code,message}}`，code ∈ `not-ready` / `root-conflict` / `root-changed` / `bad-request` / `unauthorized` / `internal`。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | `{ok,version,pid}`，免鉴权 |
| GET | `/api/state` | 权威快照：设置 + 任务 + 运行时 |
| POST | `/api/auto` | `{on}` 自动同步总开关 |
| POST | `/api/tasks` | 建任务 |
| PATCH | `/api/tasks/:id` | 改任务；换目录需 `force:true`，否则 `root-changed` |
| DELETE | `/api/tasks/:id` | 删任务（状态库保留） |
| POST | `/api/tasks/:id/sync` | 立即同步；失败 `409 not-ready` / `root-conflict` |
| POST | `/api/tasks/:id/pause` \| `/resume` | 暂停 / 恢复 |
| GET / DELETE | `/api/activities` | 记录查询 / 清空；`task=all\|<id>&status=ok\|fail&q=&limit=` |
| GET | `/api/export.csv` | 表头与 exe 导出一致 |
| GET | `/api/logs` | 内存环形日志（每任务 400 行） |
| GET | `/api/logfile` | `sync.log` 尾部 |
| POST | `/api/prune` | 按设置的保留策略清理记录 |
| GET / POST | `/api/autostart` | 开机自启（注册表值 `WinSyncDaemon`） |
| POST | `/api/open-dir` | 只能打开任务自己的根目录 |
| GET | `/api/fs/list` | 任务根内列子目录，拒绝 `..` 与绝对路径 |
| GET | `/api/events` | SSE：`hello/log/progress/done/state`，15s keepalive |

SSE 的 `state` 只是「去拉一次 `/api/state`」的提示，不携带数据；快照永远以 `/api/state` 为准。

## 调度行为

- 每 250ms 一跳，顺序执行，同一任务不会并发两轮（`running` 是唯一闸门）
- 自动同步：轮询间隔 28–34s 抖动；目录有变更则去抖 3s 后触发
- watcher 建失败（如网络盘未挂载）每 5s 重试，不影响轮询式兜底
- 进度事件按任务 200ms 节流

## 验收脚本

```bash
cargo build --bin sync-daemon --bin sync-server --bin sync-client

# 1) HTTP 契约（需要一台在跑的 daemon）
node crates/sync-daemon/scripts/smoke.mjs %LOCALAPPDATA%\dsh-winsync

# 2) 真 relay + 真 daemon + 真 CLI，跑一次双向同步
node crates/sync-daemon/scripts/e2e-sync.mjs
```

`e2e-sync.mjs` 全程用仓库里的真实二进制，不 mock 算法；断言 SSE 有 `log`/`done`、
两边文件都落地、记录入库、`runtime.last` 收尾。

## 单元测试

```bash
cargo test -p sync-daemon
```

覆盖：目录重叠判定（大小写 / 斜杠 / 父子 / 盘根）、路径穿越、CSV 转义、
时间格式、错误码、token 定长比较。
