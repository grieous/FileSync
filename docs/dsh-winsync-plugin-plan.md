# DeepSeek Harness 插件：把 WinSync 搬进 dsh

> 目标：做一个 DeepSeek Harness（dsh）插件，功能与界面与现有 `WinSync.exe`（v0.6.2）一致。
> 本文只做可行性与计划，经确认后再实施。

## 一、结论

**可行，推荐 A 方案：Rust 侧新增一个 headless 守护进程（daemon），dsh 插件负责宿主路由 + React 界面复刻。**

理由很直接：现有代码里同步算法早已是库（`sync-client` 的 `session::run` 接收事件回调，`sync-core` 的 `Store` 提供 base/哈希缓存/记录查询），`winsync-gui.rs` 那 2617 行只是壳。把壳换成 Web 壳，比在 TypeScript 里重写 BLAKE3 哈希、三方比较、WebSocket 状态机安全得多。

明确受限、做不到 1:1 的部分，先讲清楚，避免后面返工：

| 项 | 现状（exe） | 插件里 |
|---|---|---|
| 系统托盘 | `tray-icon` 常驻，关窗不退出 | dsh 无系统托盘。改为侧栏徽标 + 页内状态，不再有托盘菜单 |
| 原生选目录 | `rfd::FileDialog` | 浏览器拿不到绝对路径。改「输入框 + 服务端目录浏览（限定在已配置 root 内）+ 打开目录」 |
| 像素级还原 | egui 绘制 | 同一套色值/字号/间距/圆角，视觉高度接近，不做像素级保证 |
| 关掉 dsh 后继续同步 | 托盘常驻即可 | 默认随 dsh 退出；需要 7×24 就开「daemon 开机自启」，此时它独立于 dsh 运行 |
| 单实例 | 命名互斥体 `Local\WinSync_SingleInstance_v1` | 与 exe 共用同一互斥体，二者只能有一个在跑（见风险 R2） |

## 二、现有 exe 盘点

### 代码结构（Rust workspace，v0.6.2）

| 位置 | 行数 | 作用 | 插件方案里 |
|---|---|---|---|
| `crates/sync-proto` | 258 | 帧编解码 + 消息枚举 | 原样复用 |
| `crates/sync-core` | ~1,320 | 扫描 / BLAKE3 / 三方 diff / 状态库 / 原子落地 / 过滤 | 原样复用 |
| `crates/sync-client/src/session.rs` | 910 | 一次同步会话状态机，`run(cfg, tx, quiet, allow_root_change, wait, limits)` | 原样复用，事件流改成 SSE |
| `crates/sync-client/src/config.rs` | 393 | `config.json` 任务模型 + 旧配置迁移 + 校验 | 原样复用 |
| `crates/sync-client/src/bin/winsync-gui.rs` | 2,617 | egui 界面 + 托盘 + 自启 + 日志清理 + 自动同步调度 | **逻辑搬 daemon，界面重写** |
| `crates/sync-client/src/main.rs` | 220 | CLI（tasks / scan / sync / where） | 保留，daemon 另开子命令 |
| `crates/sync-server` | 261 | 无状态中继 | 不动 |

### 界面与能力清单（要复刻的就是这些）

- **版式**：180px 左侧栏（Logo + 同步任务 / 同步日志 / 设置 + 底部「+ 创建同步任务」），右侧内容区，深色主题。
  色值：底 `#0E0F10`、侧栏 `#161719`、卡片 `#1C1D1F`、边框 `#2A2C2E`、主色 `#2B6CF6`、成功 `#4CC38A`、失败 `#F0554D`。
- **同步任务页**：设备行（`COMPUTERNAME | 主机 · 房间`）；任务卡（文件夹图标、名称、状态、进度条、三列「电脑文件路径 / 目标房间 / 规则设置」、立即同步、上次同步时间、推送/拉取/冲突统计、圆形暂停钮、`⋯` 菜单）；自动同步开关。
- **创建向导**：三步（① 同步设备 ② 规则设置 ③ 同步路径）+ 数字步骤条 + 上一步/取消/下一步 + 「沿用已有任务的连接」。
- **任务设置弹窗**：名称 / 服务器 / 密钥（密码框）/ 房间 / 目录 + 浏览 / 模式（双向·仅上传·仅下载）/ 过滤（点文件、扩展名、大小区间）/ 规则签名 / 保存 / 暂停 / 删除。
- **同步日志页**：任务下拉（含全部任务）/ 状态下拉 / 搜索文件名 / 表头（文件名称·任务·时间·状态·事件）/ 斑马纹 / 清空记录 / 导出 CSV。
- **设置页**：开机自启（注册表 Run）、自动更新（禁用态 + 说明）、版本号、检查更新、日志保存数量与天数、上传/下载限速、诊断（导出日志、查看运行日志、打开数据目录）、当前任务编辑入口。
- **运行日志窗**：内存环形缓冲 400 行，自动贴底。
- **后台行为**：目录监听（3 秒去抖）+ 28–34 秒抖动轮询兜底；单实例；配置原子落盘；日志清理；限速在会话开始生效；删除熔断与目录一致性检查（已在 core/session 内）。

### 数据位置

- 配置：`%APPDATA%\WinSync\config.json`（`version:2, tasks:[]`，token 明文）
- 状态：`%APPDATA%\WinSync\tasks\<id>.db`（base / hash_cache / activity / device_id）
- 日志：`%APPDATA%\WinSync\sync.log`

## 三、dsh 侧事实（已在本机核对，不是推测）

- 桌面端 `0.2.1-rc.3`，profile 在 `C:\Users\gjn86\.dsh\profiles\desktop`，bundles 里已装 `dsh-better-sidebar@0.32.1`、`dsh-context@0.87.0`、`dshmarket@1.73.25`。
- 插件 = 一个 npm 包，`package.json` 里声明：
  - `dsh.bundle.patch` → 自己的 `cordis.patch.yml`（`insert` 一行挂载）
  - `dsh.client.inject` → 依赖的官方客户端包；`dsh.client.platform: "web"`
  - 客户端产物 `lib/client.js` 通过 `window.__ModuleLoader__.load({ id, factory })` 加载，React 18。
- 宿主半写法（照 `dsh-better-sidebar` 的真实源码）：
  `export const inject = ['webServer', 'tools', 'sessions', 'connection']`，路由用 `ctx.webServer.register({ kind: 'prefix', path: '/winsync/api', handler })`，工具用 `ctx.tools.register()`，全部用 `ctx.effect()` 包住以便热重载时自动撤销。
- 可用的界面挂载点（从已装插件里提取的真实 slot 名）：

| slot | 形态 | 适配度 |
|---|---|---|
| `conversation.view` | 主区视图标签（像「上下文」「Fleet」那样整块占满） | **最合适**：宽度足够放下侧栏 + 三页 |
| `sidebar.right.pane.tab` (+`.title`/`.tab.guide`) | 右侧栏标签页 | 适合放一个窄版「状态/进度」 |
| `shell.overlay` | 浮动窗（可拖拽缩放） | 像独立小工具，适合「迷你监视器」 |
| `settings.section` (+`.icon`) | 设置页分区 | 放限速、日志清理、自启 |
| `sidebar.panellist` | 侧栏面板列表 | 备用 |

→ 结论：**主区视图标签 + 设置分区 + 宿主路由 + 工具**，足以做到「功能和界面跟 exe 一样」，且比塞进右侧栏更接近原版宽敞版式。

## 四、三条路线对比

| | A. Rust daemon + 插件 UI（推荐） | B. 插件只遥控 exe | C. 纯 Node 重写同步 |
|---|---|---|---|
| 功能完整度 | 全量（含实时进度、自动同步、暂停、限速、日志清理） | 只能触发一次性同步，读配置/读库展示 | 理论全量 |
| 实时性 | SSE 推送事件，与 exe 同级 | 无进度，只能轮询结果 | 可做，但要自己实现事件 |
| 正确性风险 | 低（算法零改动） | 中（exe 与插件并发读写同一 SQLite / config） | **高**（BLAKE3、帧协议、三方 diff、原子落地全部重写） |
| 改动量 | 中：daemon ~700 行 Rust + 插件 ~300 行 TS + UI ~1,800 行 TSX | 小：~800 行 TS，但天花板低 | 大：3,000+ 行 TS，且要重跑全部回归 |
| 与 exe 共存 | 共用配置与状态库，靠互斥体串行 | 必须同时跑，风险最高 | 数据格式易漂移 |
| 维护 | 一处逻辑（Rust），两端受益 | 两套状态源 | 两份实现长期分叉 |

B 看起来最省事，但它的实质是「网页遥控器」：任务卡的进度条、上次同步统计、自动同步是否在跑，全都拿不到，最后还是要回 A。C 则是把已经踩过坑的算法重写一遍，性价比最差。

## 五、推荐架构

```
┌──────────────── dsh Web/Desktop 客户端 ────────────────┐
│  conversation.view「同步」  ← React，复刻 exe 版式        │
│  settings.section「同步」   ← 限速 / 日志清理 / 自启       │
└───────────────┬───────────────────────────────────────┘
                │ fetch /winsync/api/*  +  SSE /winsync/events
┌───────────────▼──────────── dsh 插件（Node/TS，宿主半）──┐
│  ctx.webServer.register  →  鉴权、转发、工具注册           │
│  ctx.tools.register      →  winsync_status / sync_now …  │
└───────────────┬───────────────────────────────────────┘
                │ 127.0.0.1:<随机端口>  Bearer <每次启动随机>
┌───────────────▼────────── winsync-daemon.exe ──────────┐
│  引擎循环：任务表 · 监听(3s 去抖) · 轮询(28–34s) · 限速   │
│  session::run() ← 现有同步状态机（零改动）                │
│  Store ← base / hash_cache / activity                  │
│  config.json 读写（原子） · sync.log · 日志清理 · 自启    │
└───────────────┬───────────────────────────────────────┘
                │ ws://relay（现有无状态中继）
              对端 Windows
```

### daemon 对外接口（草案）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | `{ ok, version, pid }`，插件用它判断是否要拉起 |
| GET | `/state` | 任务列表 + 每个任务 runtime（running/progress/last/last_time/next_auto）+ 全局设置 + device_id |
| POST | `/tasks` | 新建（复用 `Config` 校验） |
| PATCH | `/tasks/{id}` | 改配置（含模式/过滤/暂停）；目录变更要求 `force` |
| DELETE | `/tasks/{id}` | 删任务，状态库保留（与 exe 一致） |
| POST | `/tasks/{id}/sync` | 立即同步（后台任务，事件走 SSE） |
| POST | `/tasks/{id}/pause` · `/resume` | 暂停/恢复自动同步 |
| GET | `/activities?task=&status=&q=&limit=` | 日志页数据（`recent_activities` + 过滤） |
| DELETE | `/activities?task=` | 清空筛选结果 |
| GET | `/export.csv` | 导出（表头与 exe 一致） |
| GET | `/logs/tail?n=` | 内存环形日志 + `sync.log` 尾部 |
| GET | `/events` | SSE：`log` / `progress` / `done` / `state` |
| POST | `/open-dir` · `/autostart` · `/prune` | 打开目录、开机自启、日志清理 |
| GET | `/fs/list?path=` | 只列目录名，限定在任务 root 内（给「浏览」用） |

### 安全约束（写进验收）

1. 只绑 `127.0.0.1`，端口随机，启动时写 `%APPDATA%\WinSync\daemon.json`（含 pid/port/token，用户级 ACL）。
2. 每个请求校验 `Authorization: Bearer <token>` 与 `Origin`（只接受 dsh 自身源）。
3. token **永不**进入模型上下文：`winsync_*` 工具返回值里密钥一律打码。
4. `/fs/list` 只接受任务 root 的相对路径，复用 `sync_core::join_root` 的路径穿越防护。
5. 写配置走「临时文件 + rename」，与 exe 一致，避免写坏。

## 六、功能对齐清单

| exe 能力 | 插件实现 | 差异 |
|---|---|---|
| 左侧栏三页 + 底部新建 | 主区视图内复刻同样的 180px 侧栏 | 外层还有 dsh 自己的导航 |
| 任务卡全要素 | 同构 React 组件 | 无 |
| 进度条实时刷新 | SSE `progress` | 无 |
| 创建向导三步 | 同构，步骤条 + 校验 | 「浏览」改目录选择器 |
| 任务设置弹窗 | 同构，密钥密码框 | 无 |
| 立即同步 / 暂停 / 删除 | `POST /tasks/{id}/sync` 等 | 无 |
| 自动同步开关 | daemon 内的调度循环 | 无 |
| 日志页筛选/搜索/斑马纹/导出 | `/activities` + 前端过滤 | 无 |
| 日志清理（数量/天数） | daemon 启动与改设置时执行 | 无 |
| 限速 | `Limits` 传 `session::run` | 无 |
| 开机自启 | 仍写 HKCU Run，目标改成 daemon | 可选项 |
| 运行日志窗 | SSE + `sync.log` 尾部 | 无 |
| 托盘常驻 | 侧栏徽标 + 页内状态 | **有差异** |
| 单实例 | 共用同一命名互斥体 | 无 |
| 版本/检查更新 | 显示 daemon 版本；更新仍标注「暂不支持」 | 无 |
| （新增）agent 工具 | `winsync_status` / `winsync_sync_now` / `winsync_tasks` | 超出 exe，价值高 |

## 七、分期实施

### P0 冒烟（先证伪，再投入）
- 最小插件包：`package.json` + `cordis.patch.yml` + `lib/index.js`（空宿主）+ `lib/client.js`（在 `conversation.view` 里渲染一行字）。
- 装进 `desktop` profile，确认：能加载、能显示、热重载后能撤销、不破坏现有 `dsh-better-sidebar` / `dsh-context`。
- 同时验证 `ctx.webServer.register` 起的 `/winsync/api/health` 能通。
- **验收**：dsh 重启后「同步」标签仍在，控制台无 slot 冲突/重复路由错误。
- 风险最高的一步在这里：`lib/client.js` 的 ModuleLoader 打包格式（react 外部化、`id` 命名）必须照 `dsh-context` 的构建产物对齐。P0 不通过就不进入 P1。

### P1 winsync-daemon（Rust）
- 新 crate `crates/sync-daemon`，依赖 `sync-client`/`sync-core`，用 workspace 已有的 `axum` + `tokio`。
- 移植 GUI 里的调度与运行时逻辑：`Runtime`（running/progress/last/last_time/logs/watcher/dirty）、监听去抖、轮询抖动、退出时写日志。
- 单实例互斥体与 exe 共用；写 `daemon.json`；SSE 广播。
- **验收**：不启动 dsh，直接用 curl 跑通「建任务 → 同步 → 查记录 → 导出 CSV」；与 exe 的产出逐字节比对 `config.json` 与 activity 行。

### P2 插件宿主半（TS）
- 鉴权代理、daemon 生命周期（`/health` 失败则 spawn，退出时按设置关闭）、工具注册、设置分区数据接口。
- **验收**：`ctx.tools` 里能看到 3 个工具并返回真实数据；daemon 挂掉后 UI 自动重连并提示。

### P3 界面复刻（React）
- 设计令牌直接用 `ui_colors` 的色值；三页 + 卡片 + 向导 + 两个弹窗 + 表格。
- **验收**：与 exe 截图逐项比对（用户提供截图，或我用浏览器截 Web UI）；任务卡在三档宽度下不塌。

### P4 联调与回归
- 真实两端同步：双向 / 仅上传 / 仅下载、过滤不一致中止、删除熔断、限速生效、日志清理、CSV 导出、目录变更确认。
- 与 exe 交叉测试：exe 建的 3 个任务、旧格式迁移、并发启动互斥。
- **验收**：`cargo test --workspace` 全绿 + 上述 7 个场景实测记录写进 `docs/`。

### P5 增强（可选）
- agent 工具自然语言触发（「同步一下」→ `winsync_sync_now`）。
- 右侧栏窄版监视器 / 悬浮窗。
- 打包发布到插件市场（`dsh-plugin` topic、兼容矩阵声明）。

## 八、风险与对策

| # | 风险 | 影响 | 对策 |
|---|---|---|---|
| R1 | dsh 仍在 dev preview，客户端 slot/加载格式会变 | 插件随时可能装不上 | P0 先冒烟；锁 `compatibility.dshReleases`；client 只依赖 4 个稳定 slot |
| R2 | exe 与 daemon 同时跑 → 双写 config/DB、双份同步 | 可能误删（历史上真出过） | 共用命名互斥体；后启动者进入只读并提示「关闭 exe 或退出 daemon」；长期可把 exe 改成 daemon 的前端 |
| R3 | localhost 接口被本机其他程序/网页调用 | token 泄露、任意触发同步 | 随机端口 + 每次启动随机 Bearer + Origin 校验 + 只读 127.0.0.1 |
| R4 | 浏览器拿不到绝对路径 | 无法像 exe 那样「浏览…」 | 目录浏览 API（仅 root 内、只列目录名）+ 手输路径 |
| R5 | dsh 关闭后 daemon 成孤儿进程 | 用户以为停了，其实还在同步 | 默认「随 dsh 退出」；要常驻必须显式开自启，并在 UI 标出「独立运行中」 |
| R6 | 差异截图无法自动获取（本机沙箱禁截屏/启进程） | 视觉验收靠人 | 用浏览器预览页自测 + 请用户贴截图（与 v0.6.0 做法一致） |
| R7 | 大目录（数千文件）打开日志页卡顿 | 体验退化 | 服务端分页 + 前端虚拟列表；哈希缓存已有 82× 加速 |
| R8 | 托盘能力缺失被当成 bug | 预期不符 | 文档与设置页明说「dsh 无系统托盘」 |

## 九、明确不做（继承 exe 的已知缺口）

按需下载（Cloud Files 占位符）、断点续传、重命名优化、选择性同步子目录、带宽以外的 QoS、自动更新服务器、TLS（仍建议前置 Caddy/nginx）。

## 十、需要你确认

1. **形态**：A（推荐）/ B / C —— 我按 A 展开，B 只能做「遥控器」。
2. **界面落点**：主区视图标签（推荐，最像 exe）/ 右侧栏窄面板 / 悬浮窗 / 三个都做（先主区）。
3. **agent 工具**：要不要让 dsh 里的模型直接查状态、触发同步（默认要，但会多一层权限考虑）。

确认后我按 P0 → P1 → P3 顺序推进，每期结束给你可验证的东西，不会一口气写完再给你看。
