# Vitaloop Server

[Vitaloop](https://github.com/monkey-wenjun/vitaloop-app)（元环健康看板）的自托管同步后端：账号体系、健康快照增量同步、运动记录、地图样式/瓦片代理，以及一个只读的 **MCP（Model Context Protocol）** 接口，让 AI 助手直接读取血压、血糖、体重、睡眠、HRV 等数据。

单个静态 Rust 二进制，无第三方 crate 运行时依赖；容器基于 `scratch`。

## 功能

- **账号**：邮箱 + 密码注册/登录，加盐 60k 轮 SHA-256，30 天 Bearer 会话。
- **同步**：`GET/PUT /v1/health/data` 全量快照（原样保存 gzip 字节），`PUT /v1/health/delta` 追加增量补丁日志；服务端读取时合并。
- **运动**：`PUT /v1/health/workouts`、`GET /v1/workouts`、`GET /v1/workouts/{id}`，按账号缓存。
- **MCP**：`POST /mcp`，Streamable HTTP + JSON-RPC 2.0，14 个只读工具，复用账号 Bearer 会话。
- **地图**：`/style.json`、`/style-dark.json`、`/tiles/{z}/{x}/{y}.mvt`，上游 Protomaps PMTiles，带本地缓存。
- **博客数据**：`/blog/daily-fitness`，按 token 只读暴露一个账号的每日指标。

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

运行：

```bash
./vitaloop-server --bind 127.0.0.1:8793 --data ./data
# 或
docker run -d --name vitaloop-server -p 127.0.0.1:8793:8793 \
  -v vitaloop-data:/data vitaloop-server:latest
```

生产环境用 Nginx/Caddy 做 HTTPS 反代到 `127.0.0.1:8793`。

### 配置

| 环境变量 | 说明 |
|---|---|
| `PMTILES_URL` | 地图瓦片上游（Protomaps 月度构建），默认指向一个固定版本 |
| `PUBLIC_BASE_URL` | 对外地址；不设则按请求 `Host` / `X-Forwarded-Proto` 推断 |
| `VITALOOP_BLOG_TOKEN` | `/blog/daily-fitness` 的访问 token，不设则路由关闭 |
| `VITALOOP_BLOG_ACCOUNT_ID` | 博客路由暴露的账号 id |
| `BLOG_HEALTH_ACCOUNT_ID` | 直接指定博客账号，优先级高于上者 |

数据布局：

```
<data>/accounts/<account_id>/health.json          全量快照（可能是 gzip）
<data>/accounts/<account_id>/health.patches.jsonl 增量补丁
<data>/accounts/<account_id>/workouts.json        运动表
<data>/accounts/<id>.json                         账号记录
<data>/accounts/sessions                          token\taccount_id\texp_unix
```

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
| `query_table` | 白名单内任意只读表 |

约定：时间参数为 Unix 毫秒，日汇总用 `date_day`（epoch 天）；`limit` 默认 200、上限 2000；数值不做单位换算。结果同时返回 `structuredContent` 与等价的 `content[0].text`。

## 安全

- 仓库不包含任何部署域名、主机地址或凭据；对外地址由反向代理的 `Host` 头或 `PUBLIC_BASE_URL` 决定。
- 密码只存加盐迭代哈希；会话 token 为 32 字节随机数，落盘在数据目录，注意目录权限。
- `query_table` 只读白名单，未知表名直接拒绝。

## License

AGPL-3.0，见 [LICENSE](LICENSE)。
