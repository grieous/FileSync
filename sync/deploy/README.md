# 服务器部署

## 一句话答案

服务器上**只需要一个约 8 MB 的静态二进制文件**，加一个 systemd 服务定义。没有数据库、没有存储卷、没有 Web 服务容器。

## 要装什么 / 不要装什么

| | 内容 | 说明 |
|---|---|---|
| ✅ 必须 | `sync-server` 二进制 | 单个文件，静态链接 |
| ✅ 必须 | systemd unit | 系统自带，随二进制一起放进去 |
| ✅ 必须 | `/etc/winsync/env` | 存共享密钥，安装脚本自动生成 |
| ⚪ 可选 | nginx / Caddy | 只有需要 **wss://**（对外暴露）时才装；内网 `ws://` 不需要 |
| ❌ 不需要 | 数据库（MySQL/PG/SQLite） | 服务器不存任何数据 |
| ❌ 不需要 | Redis / 消息队列 | 配对表在内存里，进程重启重建 |
| ❌ 不需要 | 对象存储 / 挂载卷 | 文件不落盘 |
| ❌ 不需要 | Rust 工具链 | 除非你选择"在服务器上编译"（见下，用完可卸） |
| ❌ 不需要 | Docker | 可选方案，不是必须 |

**资源占用**：空闲内存约 10–20 MB，磁盘 8 MB，无 I/O。systemd 里已经限制 `MemoryMax=256M`。

## 第一步：得到二进制

三种方式，按服务器条件选：

### 方式 A：在服务器上编译（最省心，推荐）

服务器需要能上网。工具链约 700 MB，编译完可以卸载。

```bash
# 1. 装工具链（用国内镜像，否则会非常慢）
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"

# 2. 加 musl target，编出静态二进制（不依赖服务器 glibc 版本）
rustup target add x86_64-unknown-linux-musl

# 3. 编译（只编服务端，不会碰 GUI）
cd sync
cargo build --release -p sync-server --target x86_64-unknown-linux-musl

# 产物：
ls -lh target/x86_64-unknown-linux-musl/release/sync-server
```

> 用 musl 而不是默认的 gnu target，是因为静态链接后可以在任何发行版直接跑，
> 不受服务器 glibc 版本限制。服务端是纯 Rust 实现，没有 C 依赖，所以能静态编出来。

编完想清理工具链：`rustup self uninstall`（能省回约 700 MB）。

### 方式 B：本地交叉编译后上传

Windows 上交叉编译到 Linux 需要 WSL 或 Docker。**本机这两者目前都不可用**
（WSL 被安全策略拦截，Docker 未安装），所以这条路的通用做法是：
在任意一台 Linux 开发机（或 CI）上按方式 A 编好，`scp` 到服务器。

### 方式 C：Docker 构建

服务器装了 Docker 的话：

```bash
cd sync
docker build -f deploy/Dockerfile -t winsync-relay .
# 房间级密钥：每个房间一把，互不影响（必须配，否则容器直接退出）
docker run -d --name winsync-relay \
  -p 8443:8443 \
  -e SYNC_ROOM_TOKENS='{"office":"密钥A","family":"密钥B"}' \
  --restart=always \
  winsync-relay

# 想给所有房间一个兜底密钥就用 *
docker run -d --name winsync-relay \
  -p 8443:8443 \
  -e SYNC_ROOM_TOKENS='{"*":"统一密钥"}' \
  --restart=always \
  winsync-relay
```

不需要 `-v` 挂载任何目录——这个容器是无状态的。

## 第二步：安装

把二进制和 `sync-server.service`、`install.sh` 放到同一目录，然后：

```bash
chmod +x install.sh
sudo ./install.sh ./sync-server
```

脚本会：
1. 创建系统用户 `winsync`（无登录权限）
2. 二进制装到 `/opt/winsync/sync-server`
3. 生成 32 位随机密钥写入 `/etc/winsync/env`（权限 600）
4. 注册并启动 `sync-server.service`

最后会打印密钥和客户端该填的地址。**重复执行只更新二进制，不会覆盖已有密钥。**

## 第三步：放行端口

```bash
# firewalld
sudo firewall-cmd --permanent --add-port=8443/tcp && sudo firewall-cmd --reload

# ufw
sudo ufw allow 8443/tcp
```

云服务器还需要在控制台的安全组里放行 8443。

## 验证

```bash
systemctl status sync-server        # 应为 active (running)
journalctl -u sync-server -n 20     # 看启动日志
ss -lntp | grep 8443                # 确认在监听
```

能看到类似 `relay 已启动于 0.0.0.0:8443（无状态，不落盘）` 就对了。

## 常用操作

```bash
sudo systemctl restart sync-server          # 重启（客户端会自动重连）
sudo systemctl stop sync-server             # 停止
journalctl -u sync-server -f                # 跟踪日志
sudo nano /etc/winsync/env                  # 改密钥/端口后 restart
```

## 升级

```bash
sudo ./install.sh ./sync-server-new         # 直接覆盖，密钥不变
```

客户端会短暂断开并自动重连，不需要改配置。

## 什么时候需要加 TLS

| 场景 | 是否需要 wss:// |
|---|---|
| 两台 Windows 与服务器在同一内网 / VPN | 不需要，`ws://` 即可 |
| 走公网 | **建议加**，否则密钥和文件内容明文传输 |

加 TLS 最省事的做法是在前面放一个 Caddy：

```
sync.example.com {
    reverse_proxy 127.0.0.1:8443
}
```

然后客户端地址改成 `wss://sync.example.com/ws`，并把 `SYNC_ADDR` 改成 `127.0.0.1:8443`，避免绕过 TLS 直连。
