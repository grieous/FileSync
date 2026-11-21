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

## 使用

### 图形界面（推荐）

1. 启动 `winsync-gui.exe`
2. 填服务器地址（`ws://IP:8443/ws`）、密钥、房间名
3. 点"浏览…"选择要同步的文件夹
4. 保存配置 → 立即同步

两台都开着时，30 秒内自动完成。界面下方有实时日志。

### 命令行（排查用）

```bash
# 看本地状态，不联网
winsync.exe --server ws://IP:8443/ws --token 密钥 --root "D:\Sync" scan

# 执行一次同步
winsync.exe --server ws://IP:8443/ws --token 密钥 --root "D:\Sync" sync

# 打印配置与状态库位置
winsync.exe where
```

配置与 GUI 共用 `%APPDATA%\WinSync\config.json`，命令行参数可临时覆盖。
换同步目录需要加 `--force`（GUI 会弹窗确认）。

### 本机模拟两台设备

两台客户端不能共用状态库（会拿到同一个 device_id），用环境变量隔开：

```bash
WINSYNC_CONFIG=.tmp/a.json WINSYNC_DB=.tmp/a.db winsync.exe --root .tmp/a sync
WINSYNC_CONFIG=.tmp/b.json WINSYNC_DB=.tmp/b.db winsync.exe --root .tmp/b sync
```

## 实测结果

```
双向首次同步   A 推送 3 / 拉取 1      B 推送 1 / 拉取 3
第二次同步     两端一致，零传输
修改 + 删除    正确传播，内容一致
```

## 尚未实现

- 实时文件监听（`notify`），目前靠手动或 30 秒轮询
- 托盘常驻 / 开机自启 / Windows 服务
- 断点续传（4 GB 文件断开会重来）
- 重命名优化（改名会重传整个文件）
- 带宽限速、安装包、自动更新

## 两个容易踩的坑

1. **`Store` 不能跨 `await` 持有**。`rusqlite::Connection` 非 `Sync`，在 `#[tokio::main]`
   下持有 `&Store` 跨 `.await` 会编译失败。当前做法是进异步会话前把数据取出来。

2. **不要用 `mtime` 判断新旧**。时区、时钟漂移、FAT 2 秒精度都会骗人。
   `mtime` 只用于扫描阶段的快速筛选，最终一律以 `blake3` 为准。
