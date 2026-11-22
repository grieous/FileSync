# 容器化构建 + 运行。
#
# 为什么用 Docker：目标服务器是 fnos（Debian 12 定制），libc6-dev 被厂商 hold 住，
# 装编译工具链会动到系统 glibc。容器里构建完全不碰宿主系统。
#
# 只构建 sync-server：把 sync-core / sync-client 从 workspace 里剔掉，
# 避免为了编一个 8 MB 的转发器去下载 eframe 那一大堆依赖。

# 基础镜像显式走 daocloud：宿主机的 daemon.json 配了 docker.ketches.cn，那个站返回 502
FROM docker.m.daocloud.io/library/rust:1-slim AS build
WORKDIR /src

# 国内镜像：crates.io 官方源在容器里通常很慢
RUN mkdir -p /root/.cargo && printf '%s\n' \
    '[source.crates-io]' \
    'replace-with = "rsproxy-sparse"' \
    '' \
    '[source.rsproxy-sparse]' \
    'registry = "sparse+https://rsproxy.cn/index/"' \
    '' \
    '[net]' \
    'git-fetch-with-cli = true' \
    > /root/.cargo/config.toml

COPY Cargo.toml Cargo.lock ./
COPY crates/sync-proto ./crates/sync-proto
COPY crates/sync-server ./crates/sync-server

# 移除 workspace 里跟服务端无关的成员（core / client / daemon 都不进来，省依赖）
RUN sed -i '/sync-core/d; /sync-client/d; /sync-daemon/d' Cargo.toml \
 && cat Cargo.toml

RUN cargo build --release -p sync-server

FROM docker.m.daocloud.io/library/debian:bookworm-slim
RUN useradd --system --no-create-home --shell /usr/sbin/nologin winsync
COPY --from=build /src/target/release/sync-server /usr/local/bin/sync-server

USER winsync
ENV SYNC_ADDR=0.0.0.0:8443 \
    RUST_LOG=info

EXPOSE 8443
ENTRYPOINT ["/usr/local/bin/sync-server"]
