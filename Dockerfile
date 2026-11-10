# 构建阶段：静态 musl 二进制，运行阶段仍是 scratch。
FROM rust:1-slim AS builder

RUN apt-get update \
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

# 地图瓦片上游（Protomaps 月度构建），部署时可用 -e PMTILES_URL=... 覆盖。
ENV PMTILES_URL=https://build.protomaps.com/20261109.pmtiles

ENTRYPOINT ["/vitaloop-server", "--bind", "0.0.0.0:8793", "--data", "/data"]
