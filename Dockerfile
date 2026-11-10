FROM scratch

# 静态 musl 二进制，容器里没有 shell、没有包管理器，也没有 libc。
COPY vitaloop-server /vitaloop-server

VOLUME ["/data"]
EXPOSE 8793

# 地图瓦片上游（Protomaps 月度构建），部署时可用 -e PMTILES_URL=... 覆盖。
ENV PMTILES_URL=https://build.protomaps.com/20261109.pmtiles

ENTRYPOINT ["/vitaloop-server", "--bind", "0.0.0.0:8793", "--data", "/data"]
