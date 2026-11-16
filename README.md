# Vitaloop Server

[English](README.en.md) | 简体中文

[[Vitaloop](https://github.com/monkey-wenjun/vitaloop-app)（元环健康看板）的自托管同步后端：账号体系、健康快照增量同步、运动记录、地图样式/瓦片代理，以及一个只读的 **MCP（Model Context Protocol）** 接口，让 AI 助手直接读取血压、血糖、体重、睡眠、HRV 等数据。

官网（中英双语）：<https://vitaloop.awen.me/>

单个静态 Rust 二进制，无第三方 crate 运行时依赖；容器基于 `scratch`。

## 功能

- **账号**：邮箱 + 密码注册/登录，加盐 60k 轮 SHA-256，30 天 Bearer 会话。
- **同步**：`GET/PUT /v1/health/data` 全量快照（原样保存 gzip 字节），`PUT /v1/health/delta` 追加增量补丁日志；服务端读取时合并。
- **运动**：`PUT /v1/health/workouts`、`GET /v1/workouts`、`GET /v1/workouts/{id}`，按账号缓存。
- **MCP**：`POST /mcp`，Streamable HTTP + JSON-RPC 2.0，14 个只读工具，复用账号 Bearer 会话。
- **地图**：`/style.json`、`/style-dark.json`、`/tiles/{z}/{x}/{y}.mvt`，上游 Protomaps PMTiles，带本地缓存。
- **地形**：`/dem/{z}/{x}/{y}.png`，原始 terrarium DEM，给 MapLibre 的 `hillshade` / `color-relief` 用。
- **等高线**：`/contours/{z}/{x}/{y}.png`，从 DEM 现算（marching squares），透明 PNG，无需第三方样式服务。
- **博客数据**：`/blog/daily-fitness`（每日指标）与 `/blog/sleep?days=30`（每晚睡眠、
  最近一晚的分期色块、夜间 HRV / 呼吸），以及 `/blog/body?days=365`（体重、血压、血糖），
  按 `BLOG_HEALTH_ACCOUNT_ID` 只读投影一个账号。
- **星历**：`/ephemeris.zip`、`/ephemeris/status`。默认从公开 RINEX 自算 AGNSS 包；
  私有部署可设 `EPHEMERIS_CDN_URL` 指向自己的华为云 CDN，整包镜像 PGNSS + SubION + AGNSS。

## 构建

推荐直接用 Docker Compose（多阶段构建：rust 镜像内编译静态 musl 二进制，运行镜像仍是 `scratch`）：

```bash
docker compose up -d   # 构建镜像并监听 127.0.0.1:8793，数据卷 vitaloop-data 挂 /data
```

手动构建：

```bash
# 容器（Dockerfile 自带编译阶段，无需预置二进制）
docker build -t vitaloop-server:latest .

# 或本机静态 musl 二进制
cargo build --release --target x86_64-unknown-linux-musl
```

### 国内/慢网络快速部署

多阶段构建需要先拉约 300MB 的 `rust:1-slim`，出网慢时会卡很久。已有 musl
工具链的机器可以在本地编译，再把二进制打成 `scratch` 镜像，秒级完成：

```bash
cargo build --release --target x86_64-unknown-linux-musl
mkdir -p build && cp target/x86_64-unknown-linux-musl/release/vitaloop-server build/
docker build -f Dockerfile.binary -t vitaloop-server:latest build/
```

Dockerfile 内的 apt、Rustup、crates.io 已默认走清华镜像源，网络正常时多阶段构建也可用。

### 体验地址

作者自建了一个演示实例，仅供**体验功能**（看看接口长什么样、MCP 能不能连）。
**不要把真实的健康数据同步上去**：那是个人自用的小机器，随时可能重装、
清库或下线，没有备份承诺，也不做多副本。

长期使用请按下面的步骤自己私有化部署一份，数据放自己的机器上：

```bash
git clone https://github.com/monkey-wenjun/vitaloop-server.git
cd vitaloop-server
docker compose up -d --build
```

然后用自己的域名做 HTTPS 反代（见下方 nginx 示例），把域名填进 App。

运行：

```bash
./vitaloop-server --bind 127.0.0.1:8793 --data ./data
# 或
docker run -d --name vitaloop-server -p 127.0.0.1:8793:8793 \
  -v vitaloop-data:/data vitaloop-server:latest
```

生产环境用 Nginx/Caddy 做 HTTPS 反代到 `127.0.0.1:8793`。

```nginx
server {
    listen 443 ssl http2;
    server_name sync.example.com;
    # ssl_certificate ...;
    location / {
        proxy_pass http://127.0.0.1:8793;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }
}
```

### 配置

| 环境变量 | 说明 |
|---|---|
| `PMTILES_URL` | 地图瓦片上游（Protomaps 月度构建），默认指向一个固定版本 |
| `DEM_URL` | `/dem` 与 `/contours` 的 DEM 上游（terrarium 编码），默认 AWS `elevation-tiles-prod`；两者共用一份进程内缓存，同一块地形只拉一次 |
| `PUBLIC_BASE_URL` | 对外地址；不设则按请求 `Host` / `X-Forwarded-Proto` 推断 |
| `VITALOOP_BLOG_TOKEN` | 博客账号的只读 Bearer token（`/v1/workouts`、`/mcp` 用），不设则只能走会话 |
| `VITALOOP_BLOG_ACCOUNT_ID` | 博客路由暴露的账号 id |
| `BLOG_HEALTH_ACCOUNT_ID` | 直接指定 `/blog/*` 投影的账号，优先级高于上者 |
| `EPHEMERIS_UPSTREAM` | RINEX 小时文件站点根目录，用于自算星历 |
| `EPHEMERIS_RINEX` | 本地 RINEX 文件路径；设置后跳过上游下载 |
| `EPHEMERIS_CDN_URL` | 私有部署可选：自家星历 CDN 上的 `ephemeris.zip` 地址。设置后优先整包镜像，失败再退回自算源；不设则完全不访问 CDN |
| `EPHEMERIS_REFRESH_SECS` | 常驻进程的星历刷新间隔（秒，最小 60，默认 3600） |

`EPHEMERIS_CDN_URL` 只在私有部署里设置，值放 `deploy.env`（0600）或容器环境变量，
不要提交进仓库；`/ephemeris/status` 只返回来源类型 `cdn`，不会暴露域名。

数据布局：

```
<data>/accounts/<account_id>/health.json          全量快照（可能是 gzip）
<data>/accounts/<account_id>/health.patches.jsonl 增量补丁
<data>/accounts/<account_id>/workouts.json        运动表
<data>/accounts/<id>.json                         账号记录
<data>/accounts/sessions                          token\taccount_id\texp_unix
```

## 博客睡眠投影

`GET /blog/sleep?days=30`（`days` 1–365，默认 30）返回该账号最近 N 晚的睡眠，
口径与 App 首页一致：每晚按醒来日期（东八区）归组，总时长取官方锚点
`anchor_min` 之和，`sleep_bed` 提供上床/醒来时间、评分与效率，
`sleep_stages` 只用来画最近一晚的分期色块。

```json
{
  "code": 0,
  "data": [{
    "date": "2026-11-16",
    "start_time": 1794748080000, "end_time": 1794782640000,
    "duration": 32580, "deep": 7080, "light": 20820, "rem": 4680,
    "awake": 1200, "nap": 0, "score": 81, "efficiency": 93,
    "hrv_avg": 52, "hrv_min_baseline": 38, "hrv_max_baseline": 72,
    "hrv_delta": 0, "breathing_avg": 12.5
  }],
  "detail": {
    "date": "2026-11-16",
    "start_time": 1794748080000, "end_time": 1794783300000,
    "duration": 32580, "deep": 7080, "light": 20820, "rem": 4680, "awake": 1200,
    "longest_band_sec": 6300,
    "stages": [{"stage": "light", "start_ms": 1794749400000, "end_ms": 1794750660000}],
    "hrv": {"avg": 52, "min_baseline": 38, "max_baseline": 72, "delta": 0},
    "breathing": {"avg": 12.5}
  }
}
```

时长字段单位为秒（`stages` 里的时间戳是毫秒）。`data` 按醒来时间降序；
`detail` 只覆盖最新一晚，没有数据时是 `{"has_data": false}`。

## 博客身体指标投影

`GET /blog/body?days=365`（`days` 1–3650，默认 365）把快照里的
`weight_records`、`blood_pressure_records`、`blood_glucose_records` 按 App 的
去重规则投影出来：体重 60 秒内同读数合并并互补体成分字段，血压 1 秒内同读数
合并，血糖同一分钟桶内同值合并；同时丢弃明显不合理的读数（体重 20–400 kg、
收缩压 50–250、舒张压 30–180、血糖 1–35）。

```json
{
  "code": 0,
  "data": {
    "weight": [{"id": "...", "timestamp_ms": 1794053566466, "weight_kg": 68.2,
                "bmi": 22.1, "body_fat_percent": 18.4, "source": "..."}],
    "blood_pressure": [{"id": "...", "timestamp_ms": 1793146149979,
                        "systolic": 118, "diastolic": 76, "pulse": 61}],
    "blood_glucose": [{"id": "...", "timestamp_ms": 1786300000000,
                       "value": 5.6, "measure_type": "FASTING"}]
  }
}
```

行是快照里的原始列名，未做单位换算；`weight` 保留完整 23 列体成分字段。

## MCP

`POST /mcp`，`Authorization: Bearer <session>`。一条请求一条 JSON-RPC 响应，不用 SSE；支持协议版本 `2024-11-05` / `2025-03-26` / `2025-06-18`。

```bash
curl -sS -X POST http://127.0.0.1:8793/mcp \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'

curl -sS -X POST http://127.0.0.1:8793/mcp \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call",
       "params":{"name":"blood_pressure","arguments":{"from_ms":1700000000000,"limit":20}}}'
```

工具（全部只读）：

| 工具 | 内容 |
|---|---|
| `health_summary` | 近 N 天总览（体重/血压/血糖/睡眠/步数/静息心率/HRV/血氧/压力/体温） |
| `blood_pressure` | 血压记录 |
| `blood_glucose` | 血糖记录 |
| `weight` | 体重与体成分 |
| `temperature` | 腕温与核心体温 |
| `metric_points` | 通用指标序列（`skin_temp`、`body_temp`、`resting_hr` 等） |
| `hrv` | HRV 记录 |
| `stress` | 每日压力 |
| `spo2` | 每日血氧 |
| `heart_rate` | 心率采样点（默认最近 24 小时） |
| `sleep` | 睡眠会话、卧床/评分与分期 |
| `daily_metrics` | 每日汇总 |
| `workouts` | 运动记录 |
| `query_table` | 白名单内任意只读表，含 `ecg_records` / `ecg_samples` |

约定：时间参数为 Unix 毫秒，日汇总用 `date_day`（epoch 天）；`limit` 默认 200、上限 2000；数值不做单位换算。结果同时返回 `structuredContent` 与等价的 `content[0].text`。

## 安全

- 仓库不包含任何部署域名、主机地址或凭据；对外地址由反向代理的 `Host` 头或 `PUBLIC_BASE_URL` 决定。
- 密码只存加盐迭代哈希；会话 token 为 32 字节随机数，落盘在数据目录，注意目录权限。
- `query_table` 只读白名单，未知表名直接拒绝。

## License

AGPL-3.0，见 [LICENSE](LICENSE)。
