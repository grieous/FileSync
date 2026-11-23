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

### 装 tgz（推荐，自带 daemon）

```bash
dsh plugin --profile desktop add file:C:\path\to\dsh-winsync-0.5.2.tgz
```

包内带的是 **`bin/winsync-daemon.exe.gz`**（6.2 MB 的 PE32+ x86-64，压完 2.8 MB），不是裸 exe。
首次启动时展开到 `%LOCALAPPDATA%\dsh-winsync\bin\winsync-daemon.exe`，之后 daemon 一直在那儿跑。

> 为什么不直接把 exe 塞进 `node_modules`：pnpm 覆盖安装是「删掉旧目录再重链」，而**正在运行的
> daemon 会占住那个 exe**，删不掉就报
> `[ERR_PNPM_EPERM] importPackage ... dsh-winsync_tmp_<pid>_<n>\bin`。
> 运行点挪到状态目录后，`node_modules` 里只剩一个数据文件，覆盖安装退化成普通文件替换；
> 顺带也不会被杀软按「可执行文件写入 node_modules」拦下来。

**从 0.5.1 及更早覆盖安装的这一次**，旧包里的裸 exe 可能还占着，要先清一次：

```powershell
Get-Process winsync-daemon -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item -Recurse -Force "$env:USERPROFILE\.dsh\profiles\desktop\node_modules\dsh-winsync" -ErrorAction SilentlyContinue
Get-ChildItem "$env:USERPROFILE\.dsh\profiles\desktop\node_modules" -Filter "dsh-winsync_tmp_*" -Directory |
  Remove-Item -Recurse -Force
```

装完重启 dsh，左侧栏出现「文件同步」。

### 指向仓库目录（开发用）

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
3. **包内种子 `bin\winsync-daemon.exe.gz`** → 展开到 `<state>\bin\winsync-daemon.exe` 再运行
4. 包内裸 `bin\winsync-daemon.exe`（自制包/手工塞进去的，直接跑，不做种子）
5. `<state>\bin\winsync-daemon.exe`（手工安装位，也是种子展开的目标）
6. `%LOCALAPPDATA%\Programs\WinSync\winsync-daemon.exe`
7. `PATH` 里的 `winsync-daemon.exe`
8. 仓库开发产物 `sync/target/debug|release/sync-daemon.exe`

1、2 是显式指定，优先级最高，也**不触发种子展开**（测试与自制构建靠这个）。
种子按 sha256 判断要不要重展开：同一份只解压一次，换了包才覆盖；覆盖失败（旧副本正在跑）
就继续用旧副本，并在 `/diag` 的 `daemon.seed` 里标出 `error`，不会因此起不来。

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
| 侧栏底部 | 版本角标 `v0.5.3 · c76a83c`，点击复制完整版本信息 |

响应式用容器查询，因为面板宽度取决于 dsh 的分栏而不是视口：> 820px 三列，≤ 820px 两列，
≤ 640px 单列且侧栏收窄到 132px。

面板只占 dsh 主区那一格：dsh 的左侧栏、Windows 标题栏和菜单始终露在外面，离开就点 dsh 侧栏里的会话
（面板内不再放「返回 dsh」，那类按钮在多面板宿主里是多余的）。

几个界面约定：

- 左栏入口叫 **「文件同步」**，避免和 dsh 自己的「同步」概念混淆；入口图标是 18px 线条字形，跟随侧栏
  `currentColor`，不画品牌色底块；
- **服务器地址只填 IP 或 IP:端口**（如 `192.168.3.222` 或 `192.168.3.222:18443`），协议头、`/ws`
  和不填端口时的默认 8443 自动补全；输入框里也只回显 `host:port`，老配置里的完整 `ws://…/ws` 照样接受；
- 同步日志表格带 **序号**，文件列显示**磁盘绝对路径**（任务目录 + 相对路径），长路径换行不省略；
- **人只需要交换一样东西：6 位通道码**。房间码 32 位、密钥、邀请串默认都收在「高级配置」里
  并且打码显示（点「显示内容」才看得到）；要把配置发给别人，用「复制邀请」，它带的是
  `ws://192.168.3.222:18443/ws?pair=<通道码>`，对方粘进「加入已有同步」就能填好地址与通道码。
  完整的 `?workspace=<房间码>&token=<密钥>` 邀请串仍然在高级配置里，给不填通道码、直接粘邀请的人用。
- **第一步必须先配对成功才能往下走**。配对由 daemon 长期驻守完成（浏览器不连中继：
  面板跑在 dsh 页面里，CSP 与混合内容会把 `ws://` 掐掉，旧版「配对永远连不上」就是这个原因）。
  守护连接进的是旁路房间 `<通道码>::pair`：中继按房间名配对、一间只容 2 端，直接挤进真房间
  会和正在同步的 daemon 抢位，还可能被对方当成同步对端白等一轮。
  - 两端填同一个 6 位通道码 → 中继各推一条 `PeerStatus`，界面变「✓ 配对已通过」并放行；
    同时**通道码输入框置灰锁住**——它已经完成使命，还能改只会把对端甩在半路；
  - 只有一端 → 显示「⏳ 已连上中继，正在等对方」，「下一步」保持禁用；
  - 密钥不对 / 连不上 → 明确报错并禁用；对端一离开就退回等待、重新禁用。
  - 唯一出口：「对方已经建好任务了？先建我这边」——先建的人不会被永远锁在第一步，
    但要显式点一下，并会提示「已跳过配对等待」。
- **32 位房间码靠配对通道传过去**：6 位码只负责见面，拿它反推房间等于把房间门禁降到 100 万种，
  所以创建端把 `房间码|密钥` 交给 daemon，daemon 编进 `Hello` 的 device_id，
  中继再原样塞进 `PeerStatus` 推给另一端——加入端因此不用手抄 32 位随机码。
  中继全程不落盘，也不认识这段载荷。
- **房间码与密钥**：新建任务自动生成 32 位随机房间码（不用起名字、不用去中继登记），
  默认密钥就是房间码；中继启动时若配了 `SYNC_ROOM_TOKENS`，才需要把「密钥」填成那一把。
  注意：开了房间级密钥后，配对通道（`<通道码>::pair`）也要放行，否则第一步就会报「中继拒绝了配对通道」，
  给 `SYNC_ROOMS` 加一条 `"*"` 兜底密钥最省事。
- **跟随 dsh 主题**：读 `body[data-ds-dark-theme]`，深浅色自动切换（换肤只改一组 CSS 变量）；
  按钮、提示条、横幅的底色/描边/文字都走这套变量，不写死颜色——浅色主题下深底浅字会被当成禁用；
- **新建任务分两个入口**：「加入已有同步」（填对方给的 6 位通道码，或粘邀请串）和
  「创建新同步」（自动生成通道码与房间码，一键复制邀请）；任务卡菜单里也能随时「复制邀请」。
- **中继地址记在本机**：填过并配对成功（或建过任务、改过任务）的地址存在 localStorage
  （键 `winsync.relay`），下次新建任务直接带出并注明「已带出上次用的中继地址」。
  只记地址——房间码、密钥、通道码一律不落盘。换台机器本来就该重新填一次。
- **同步目录不存在会自动创建**（新建任务、保存修改时都会），不用先去资源管理器建目录。
  启动同步时故意**不**自动建：盘符没挂上时把空目录当真，会把「本地全没了」传给对端。
- **侧栏底部常驻版本角标**（`v0.8.0`，只报版本，不显示 git 记录）。`/version` 不碰 daemon，所以同步服务起不来时
  它照样显示——「装了新版界面却没变」这类问题，先看角标上的版本与提交，再决定要不要查安装。
  点一下复制版本、打包时间与安装目录。

**与 exe 的差异**：浏览器拿不到系统的目录选择框（`<input type="file" webkitdirectory>` 只给相对路径），
所以创建向导第二步是**粘贴绝对路径**，hint 里写明了原因。

## 打包

改完插件代码后出一份 tgz（产物落在 `dist/dsh-winsync-<version>.tgz`）：

```bash
npm run pack
```

版本号按 patch 递增；安装到别的机器用 `dsh plugin --profile <名> add <tgz 路径>`。

打包脚本会做四件额外的事：把 `bin/winsync-daemon.exe` 压成 `bin/winsync-daemon.exe.gz` 放进包
（tgz 约 2.8 MB）、**解压回读校验 sha256**、**拒绝任何裸 exe 进包**——裸 exe 就是
`ERR_PNPM_EPERM` 的来源，缺文件或体积异常也直接失败，不会产出一个装完才发现少东西的包；
最后把 `builtAt` 写进包里的 `package.json`，面板的版本角标只显示版本号与打包时间
（用户机器上没有 `.git`，也不该让人对着提交号猜）。

**正式发布请先提交源码再打包**，包里的源码与提交对得上再发；打完再把 `dist/` 单独提交。

如果偏好里设了 `binPath` 或环境变量 `WINSYNC_DAEMON_BIN`，会优先用那些。

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
| `GET /version` | 插件自述：`version` / `builtAt` / `dir`。**不碰 daemon**，所以同步服务起不来时也能回答「装的是哪一版」 |
| `GET /ping` | 插件自检：状态目录、daemon 快照、版本与 commit；不含 token |
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
| 覆盖安装报 `ERR_PNPM_EPERM … dsh-winsync_tmp_<pid>_<n>\bin` | daemon 是不是正在从 `node_modules\dsh-winsync\bin` 里跑（`Get-Process winsync-daemon`），或杀软在拦 `node_modules` 里的 exe | 0.5.2 起运行点是 `<state>\bin`，新装不会再犯。从 0.5.1 及更早升级的这一次先退 dsh、`Stop-Process -Name winsync-daemon -Force`、删掉旧 `node_modules\dsh-winsync`（含 `dsh-winsync_tmp_*`）再装 |
| 左侧栏没有「同步」 | `dsh plugin --profile <p> list` 里有没有 `dsh-winsync`；浏览器 console 有没有模块加载错误 | 少东西就重装：删 `~/.dsh/profiles/<p>/node_modules/dsh-winsync` 再 `dsh plugin --profile <p> install --force` |
| 面板打开是「同步服务不可用」 | 面板「设置 → 诊断」里的 `daemon` 段；`%LOCALAPPDATA%\dsh-winsync\daemon.json` 是否存在 | 点「重试」；仍失败就看 daemon 日志（`<state>\logs\`） |
| 第一步一直「正在等对方」 | 对方还没打开向导，或填的不是同一个 6 位通道码 | 把通道码念给对方，或用「复制邀请」发整串；两端都进同一个配对通道后这边自动通过 |
| 第一步报「中继拒绝了配对通道」 | 中继启动时配了 `SYNC_ROOM_TOKENS`（房间级密钥），而配对通道 `<通道码>::pair` 不在名单里 | 给 `SYNC_ROOMS` 加一条 `"*"` 兜底密钥，或临时关掉房间级密钥 |
| 第一步报「连不上中继」 | 地址是不是只写了 IP、端口被补成默认 8443 | 写成 `192.168.3.222:18443` 这种带端口的；配对由 daemon 发起，能排除浏览器 CSP / 混合内容的影响 |
| 任务卡一直是「等待对端上线」 | 对方还没建任务，或那边的 dsh 没在运行 | 这是正常等待，不是失败；对方加入后状态会变成「同步中」→「两端已一致」 |
| 提示 `daemon-missing` | 诊断里的 `daemon.seed`、包内 `bin\winsync-daemon.exe.gz`、`<state>\bin\`、`Program Files\WinSync\`、`PATH` | `cargo build -p sync-daemon --release` 后把 exe 放进 `<state>\bin\`，或重新 `npm run pack` 打一个自带种子的包，或在设置里填绝对路径 |
| 诊断里 `daemon.seed.error` 是 `EBUSY`/`EPERM` | 状态目录里的旧 exe 正在跑，种子换不上去 | 正常（沿用旧副本）；要让新 exe 生效就退 dsh 再启动一次 |
| 改了界面却不生效 | **先看侧栏底部的版本角标**：版本与 commit 是不是你以为的那一版 | 客户端 bundle 在页面加载时注入，**必须重启 dsh**；重装前记得删旧的 `node_modules/dsh-winsync`（pnpm 会硬链旧快照） |
| 「功能看着像丢了」/ 不确定装的是哪一版 | 侧栏底部角标；或 `curl http://127.0.0.1:<port>/winsync/api/version`；`/ping` 与 `/diag` 也带版本 | 版本号对不上就是旧快照，删 `node_modules/dsh-winsync` 重装并重启 dsh |
| 进了同步页退不出来 | 点 dsh 左侧栏里的任意会话，或按 `Ctrl+B` 收起/展开侧栏 | 主区槽由宿主 `layout.selectPanel(null)` 接管；插件不再自带返回按钮 |
| 同步「看着很慢」 | 设置里的限速是不是非 0 | 1.5 MB @ 256 KB/s 实测 6.6 秒。旧版本把 KB/s 当 kbit/s，慢 8 倍，见 `sync/README.md` |
| 报 `Connection reset without closing handshake` | 房间是不是已经满了（一个房间最多 2 端） | 中继发完 `Bye` 立刻断 socket，客户端只看得到协议错误。先确认没有第二个进程/任务连同一个房间；对端残留的旧会话要重启 exe 才会断 |
| 报 `HTTP error: 401 Unauthorized` / 「密钥与服务器不一致」 | 任务里的密钥与中继 `rooms.json` 里该房间的那把是否一致 | 在任务「设置」里填「密钥」（留空 = 跟随房间码），点「显示」可回看当前值；中继没配房间密钥时这一项本来就不校验 |
| 报 `连接服务器失败（ws://host:8443/ws）` 且是 10061 | 地址里没写端口，被补成了默认 8443 | 在服务器地址里写 `host:18443` 这种带端口的；输入框下面会实时显示「将连接 …」 |

## 测试

```bash
node test/client-smoke.mjs   # 客户端注册契约（main 面板 + 左栏行 + 返回对话出口）
node test/p2-smoke.mjs       # 宿主：路由、代理、CRUD 透传、SSE、工具、卸载清理、/diag 脱敏
node test/seed-binary.mjs    # 包内种子展开：字节一致、幂等、换种子重展开、坏种子不致命
node test/version.mjs        # /version：不依赖 daemon、版本与 package.json 一致、不暴露 git 记录
node test/version-ui.mjs     # 版本角标（需 react/jsdom）：只显示版本、复制、接口挂了不拖垮面板
node test/pairing.mjs        # 配对（需 react/jsdom）：6 位通道码、等对端门禁、通过后置灰锁定、401/连不上拦截、配置从见面礼带过来
node test/announce.mjs       # 配对载荷（真中继 + 真 daemon）：device_id 载配置、PeerStatus 推回、两端互看
node test/cdp-inspect.mjs <url> [port] [--aria=同步]   # 真实页面里的渲染与截图

# 同步本体（双端、安全阀、限速、与 exe 的隔离）在仓库根的 sync/tools/
node ../sync/tools/e2e.mjs   # 77 项断言：真 relay + 两个真对端，约 25 秒

# 分步视觉走查（需要先起一个实例）：空态→向导三步→任务卡→日志→设置→窄态
CDP_SIZE=1440,960 node test/cdp-p3.mjs "<url>" [port]   # 截图落在 test/shots/
```

CDP 脚本一律用真实鼠标事件（`Input.dispatchMouseEvent`）而不是 DOM 的 `.click()`：
面板里「+ 创建同步任务」这类文字在父 `div` 上也有，`.click()` 会点到没有 onClick 的那一层。
