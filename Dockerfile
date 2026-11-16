# 构建阶段：静态 musl 二进制，运行阶段仍是 scratch。
FROM docker.m.daocloud.io/library/rust:1-slim AS builder

ENV RUSTUP_DIST_SERVER=https://mirrors.tuna.tsinghua.edu.cn/rustup \
    RUSTUP_UPDATE_ROOT=https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup

RUN set -eux; \
    for sources in /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources; do \
      if [ -f "$sources" ]; then \
        sed -i \
          -e 's|deb.debian.org|mirrors.tuna.tsinghua.edu.cn/debian|g' \
          -e 's|security.debian.org|mirrors.tuna.tsinghua.edu.cn/debian-security|g' \
          "$sources"; \
      fi; \
    done; \
    mkdir -p /usr/local/cargo; \
    printf '%s\n' \
      '[source.crates-io]' \
      'replace-with = "tuna"' \
      '' \
      '[source.tuna]' \
      'registry = "sparse+https://mirrors.tuna.tsinghua.edu.cn/crates.io-index/"' \
      > /usr/local/cargo/config.toml; \
    apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --target x86_64-unknown-linux-musl

FROM scratch

# 静态 musl 二进制，容器里没有 shell、没有包管理器，也没有 libc。
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/vitaloop-server /vitaloop-server

VOLUME ["/data"]
EXPOSE 8793

# 地图瓦片上游（Protomaps 每日构建，只保留最近几天）。
# {date} 会在启动时解析成最近可用的一份，避免写死日期过期后 404。
# 部署时可用 -e PMTILES_URL=... 覆盖成固定地址。
ENV PMTILES_URL=https://build.protomaps.com/{date}.pmtiles

ENTRYPOINT ["/vitaloop-server", "--bind", "0.0.0.0:8793", "--data", "/data"]
