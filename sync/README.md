# WinSync — 两台 Windows 经 Linux 中继双向同步

设计文档：[`../docs/windows-sync-design.md`](../docs/windows-sync-design.md)
架构图：[`../docs/architecture.svg`](../docs/architecture.svg)

服务器**不存储任何数据**，只做无状态配对与帧转发：不建库、不挂卷、可随时重启。
代价是两端必须同时在线（详见设计文档 4.2）。

## 目录结构

```
sync/
├── crates/
│   ├── sync-proto/    消息与帧编解码（+ 单元测试）
│   ├── sync-core/     扫描 · 哈希 · 三方比较 · 状态库 · 文件落地（+ 单元测试）
│   ├── sync-server/   无状态中继
│   └── sync-client/
│       ├── src/main.rs              CLI（排查用）
│       └── src/bin/winsync-gui.rs   图形界面 ← 主入口
```

## 构建

```bash
cargo build --release            # 产出 target/release/winsync-gui.exe
cargo test  --workspace          # 单元测试
node tools/e2e.mjs               # 端到端实测：真 relay + 两个真对端（见下）
```

### 本机开发环境（已验证）

| 项目 | 版本 |
|---|---|
| rustc / cargo | 1.100.0 |
| 工具链 | `stable-x86_64-pc-windows-msvc` |
| linker | MSVC 14.50.35717（VS 18 Community）+ Windows SDK 10.0.26100 |

`cargo build --workspace` 通过，端到端同步已实测（见下）。

### 环境相关的两件事

1. **cargo 源已指向 rsproxy**（`C:\Users\gjn86\.cargo\config.toml`）。
   官方源实测约 14 KB/s，装工具链要十几分钟；镜像约 3.9 MB/s。恢复官方源删掉该文件即可。

2. **Windows 控制台代码页**默认 cp936，Rust 按 UTF-8 输出中文会乱码。
   CLI 启动时已调 `SetConsoleOutputCP(65001)`；GUI 不需要考虑，字体另行加载了系统中文字体。

## 鉴权：每间房一把密钥（必填）

中继只做隧道（按房间名配对、原样转发帧、不落盘），但**必须配密钥**——没配直接拒绝启动，
不会出现「谁都能进」的裸跑。

```json
// /etc/winsync/rooms.json
{ "office": "密钥A", "family": "密钥B" }
```

```bash
SYNC_ROOMS=/etc/winsync/rooms.json sync-server
# 容器里不方便挂文件时直接给 JSON：
SYNC_ROOM_TOKENS='{"office":"密钥A"}' sync-server
```

- 表里的 `"*"` 可以作为兜底密钥：`{"*":"密钥A","vip":"密钥B"}`，没单独配的房间用 `*`；
- 查不到密钥（且没有 `*`）的房间一律 401——避免「配了一半」变成敞口；
- 改了文件不用重启，中继按 mtime 自动重读；
- 客户端**必须**填密钥，填的就是自己那一间房对应的那把。

## 使用

### 图形界面（推荐）

1. 启动 `winsync-gui.exe`
2. 填服务器地址（只填 `IP` 或 `IP:端口` 即可，默认 8443）、**该房间的密钥**、房间名
3. 点"浏览…"选择要同步的文件夹
4. 选同步模式（双向 / 仅上传 / 仅下载），按需展开「过滤规则」
5. 保存配置 → 立即同步

界面分三页：同步 / 记录（文件级操作）/ 日志。两台都开着时一次会话通常不到一秒。

### 命令行（排查用）

```bash
# 看本地状态，不联网（会打印生效的模式与过滤规则签名）
winsync.exe --server ws://IP:8443/ws --token 密钥 --root "D:\Sync" scan

# 执行一次同步
winsync.exe --server ws://IP:8443/ws --token 密钥 --root "D:\Sync" sync

# 等待对端的时间（默认 300 秒）
winsync.exe ... sync --wait 30

# 打印配置与状态库位置
winsync.exe where
```

模式与过滤规则属于持久配置，命令行不覆盖；改 `config.json` 或界面即可，例如：

```json
{
  "server": "ws://192.168.3.222:18443/ws",
  "token": "…",
  "workspace": "default",
  "root": "D:\\Sync",
  "mode": "upload_only",
  "filter": { "ignore_dotfiles": true, "ignore_extensions": ["lnk", "pst"] }
}
```

配置与 GUI 共用 `%APPDATA%\WinSync\config.json`，命令行参数可临时覆盖。
换同步目录需要加 `--force`（GUI 会弹窗确认）。

### 本机模拟两台设备

两台客户端不能共用状态库（会拿到同一个 device_id），用环境变量隔开：

```bash
WINSYNC_CONFIG=.tmp/a.json WINSYNC_DB=.tmp/a.db winsync.exe --root .tmp/a sync
WINSYNC_CONFIG=.tmp/b.json WINSYNC_DB=.tmp/b.db winsync.exe --root .tmp/b sync
```

## 端到端实测（`tools/e2e.mjs`）

单元测试管不到的东西——两端的会话配对、帧序、文件真正落地、状态库推进——
靠这个脚本跑。它**不 mock 任何东西**：起一个真 `sync-server`，两个真 `sync-client`
（加上 `sync-daemon` 的 HTTP 路径），每个场景一个独立房间与独立状态目录。

```bash
node tools/e2e.mjs                      # 全部场景，约 25 秒（含一次限速实测）
node tools/e2e.mjs --only fuse          # 只跑名字含 fuse 的场景
node tools/e2e.mjs --bin release        # 用 release 产物
node tools/e2e.mjs --keep               # 保留 .tmp/p4，事后翻文件
E2E_VERBOSE=1 node tools/e2e.mjs        # 打印每一轮同步的完整输出
```

| 场景 | 断言 |
|---|---|
| `bidir` | 推 / 拉 / 删除传播同时成立；再跑一轮必须「两端已一致」（零传输） |
| `upload` / `download` | 只上传 ⇄ 只下载（两种角色都测）：文件只朝一个方向流，反向文件不会漏出去 |
| `conflict` | 两端改同一文件：较新者胜，败方只留 **一份** 冲突副本，且副本不参与同步 |
| `filter-mismatch` | 两端都中止并各自打印双方签名，文件零改动 |
| `delete-fuse` | 删 1 项放行；删 24 项两端中止，提示要删多少，对端文件一个没少 |
| `root-change` | 换目录不带 `--force` 拒绝执行；带 `--force` 才重建基线 |
| `daemon-cli` | daemon(HTTP) ⇄ CLI 互操作：双向落地、`/api/export.csv` 表头与转义、`PATCH /api/settings` 只改传入字段、SSE 连接中仍能干净退出 |
| `rate-limit` | 256 KB/s 传 1.5 MB 实测 6.4s（理论 6.1s）；解除限速后 0.27s |
| `isolation` | exe 已同步目录（含父目录包含）→ 409 `root-conflict`；exe 的 `config.json` 内容与 mtime 都不动 |

当前结果：**77 项断言全过**（debug / release 各一遍）；`cargo test --workspace` 41 项全绿。

联调抓到的真问题：**限速把 KB/s 当成了 kbit/s**（`bytes*8/(kbps*1024)`），
256 KB/s 实际只有 32 KB/s——1.5 MB 要 48 秒而不是 6 秒。已改成
`bytes/(kbps*1024)` 并补了两条单测（`seconds_for`）。

## 实测结果（早期手工验收）

```
双向 + 过滤     A 索引 3 条（.env/.git/.lnk 被过滤）→ B 收到 2 个文件，0 删除
仅上传/仅下载   A 只推不入，B 只入不推；切回双向后 0 删除、0 误判
过滤不一致      两端各自中止并打印双方签名，无任何文件变动
心跳            服务端空闲阈值 25 秒，单端等待 70 秒不中断
```

## 尚未实现

- **按需下载（Windows Cloud Files 占位符 / 水合）** —— 与成熟网盘客户端差距最大的一项
- 选择性同步（只选子目录）
- 托盘常驻 / 开机自启 / Windows 服务
- 断点续传（大文件断开会重来）
- 重命名优化（改名会重传整个文件）
- 安装包、自动更新
- TLS 内建（目前明文 `ws://`，公网需前置 nginx / Caddy）

已完成的关键保护：目录实时监听（`notify`，3 秒去抖 + 30 秒轮询兜底）、删除熔断（单次 >20 个或 >基线 30% 即中止）、配置校验与 BOM 兼容、日志落盘 `%APPDATA%\WinSync\sync.log`、单实例互斥。

## 排查

```powershell
# 版本、设备身份、配置路径
winsync-cli.exe where
winsync-cli.exe scan

# 运行日志（每次连接、配对、推送/拉取计数）
Get-Content "$env:APPDATA\WinSync\sync.log" -Tail 20
```

服务端日志里应该能看到两台**不同**的 device_id 各自加入房间并「配对成功」。只出现一个，说明另一台根本没发起连接（配置不完整或目录不存在）。

## 三个容易踩的坑

1. **`Store` 不能跨 `await` 持有**。`rusqlite::Connection` 非 `Sync`，在 `#[tokio::main]`
   下持有 `&Store` 跨 `.await` 会编译失败。当前做法是进异步会话前把数据取出来。

2. **不要用 `mtime` 判断新旧**。时区、时钟漂移、FAT 2 秒精度都会骗人。
   `mtime` 只用于扫描阶段的快速筛选，最终一律以 `blake3` 为准。

3. **换同步目录会清空共识基线，对端会把「新目录里没有的文件」当成你删了**。
   大目录会被删除熔断拦住（单次 >20 项或 >30%），小目录则静默消失。
   两台机器要换目录就一起换，或者先确认新目录里已经包含旧目录的内容。

