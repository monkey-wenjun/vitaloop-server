# Vitaloop Server

English | [简体中文](README.md)

Self-hosted sync backend for [[Vitaloop](https://github.com/monkey-wenjun/vitaloop-app)
(an open-source health dashboard): accounts, incremental health snapshot sync,
workout records, map style/tile proxying, and a read-only **MCP (Model Context
Protocol)** endpoint that lets AI assistants read blood pressure, blood glucose,
weight, sleep, HRV, and other data.

Website (English / 简体中文): <https://vitaloop.awen.me/>

A single static Rust binary with no third-party crate at runtime; the container
image is based on `scratch`.

## Features

- **Accounts**: email + password registration/login, salted 60k-round SHA-256,
  30-day bearer sessions.
- **Sync**: `GET/PUT /v1/health/data` full snapshots (stored verbatim as gzip
  bytes) and `PUT /v1/health/delta` append-only patch logs; the server merges
  them on read.
- **Workouts**: `PUT /v1/health/workouts`, `GET /v1/workouts`,
  `GET /v1/workouts/{id}`, cached per account.
- **MCP**: `POST /mcp`, Streamable HTTP + JSON-RPC 2.0, 14 read-only tools,
  reusing the account bearer session.
- **Maps**: `/style.json`, `/style-dark.json`, `/tiles/{z}/{x}/{y}.mvt`, backed
  by upstream Protomaps PMTiles with a local cache.
- **Terrain**: `/dem/{z}/{x}/{y}.png`, raw terrarium DEM for MapLibre
  `hillshade` / `color-relief`.
- **Contours**: `/contours/{z}/{x}/{y}.png`, computed from the DEM on the fly
  (marching squares) as transparent PNGs — no third-party terrain service
  needed.
- **Blog data**: `/blog/daily-fitness` (daily metrics) and `/blog/sleep?days=30`
  (per-night sleep, the latest night's stage bands, night HRV / breathing),
  read-only projections of the account named by `BLOG_HEALTH_ACCOUNT_ID`.
- **Ephemeris**: `/ephemeris.zip`, `/ephemeris/status`, an AGNSS package computed
  from public RINEX data.

## Build

Docker Compose is the recommended path (multi-stage build: the static musl
binary is compiled inside a rust image, and the runtime image is still
`scratch`):

```bash
docker compose up -d   # builds the image and listens on 127.0.0.1:8793, data volume vitaloop-data mounted at /data
```

Manual build:

```bash
# Container (the Dockerfile contains the build stage, no prebuilt binary needed)
docker build -t vitaloop-server:latest .

# Or a static musl binary on the host
cargo build --release --target x86_64-unknown-linux-musl
```

### Fast builds on slow networks

The multi-stage build first pulls a ~300 MB `rust:1-slim` image, which can stall
on slow links. On a machine that already has a musl toolchain, compile locally
and pack the binary into a `scratch` image — this takes seconds:

```bash
cargo build --release --target x86_64-unknown-linux-musl
mkdir -p build && cp target/x86_64-unknown-linux-musl/release/vitaloop-server build/
docker build -f Dockerfile.binary -t vitaloop-server:latest build/
```

apt, Rustup, and crates.io inside the Dockerfile default to the Tsinghua mirror;
the multi-stage build works fine when the network is healthy.

### Demo instance

The author runs a demo instance for **trying the API only** (to see what the
endpoints look like and whether MCP connects). **Do not sync real health data to
it**: it is a small personal machine that may be rebuilt, wiped, or taken
offline at any time, with no backup guarantee and no replication.

For long-term use, deploy your own copy as below and keep the data on your own
machine:

```bash
git clone https://github.com/monkey-wenjun/vitaloop-server.git
cd vitaloop-server
docker compose up -d --build
```

Then put it behind HTTPS on your own domain (see the nginx example below) and
enter that domain in the app.

Running:

```bash
./vitaloop-server --bind 127.0.0.1:8793 --data ./data
# or
docker run -d --name vitaloop-server -p 127.0.0.1:8793:8793 \
  -v vitaloop-data:/data vitaloop-server:latest
```

In production, terminate HTTPS with Nginx or Caddy and proxy to
`127.0.0.1:8793`:

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

### Configuration

| Variable | Description |
|---|---|
| `PMTILES_URL` | Map tile upstream (Protomaps monthly build); defaults to a pinned version |
| `DEM_URL` | DEM upstream for `/dem` and `/contours` (terrarium encoding), defaults to AWS `elevation-tiles-prod`; both share one in-process cache, so each terrain tile is fetched once |
| `PUBLIC_BASE_URL` | Public base URL; inferred from the request `Host` / `X-Forwarded-Proto` when unset |
| `VITALOOP_BLOG_TOKEN` | Read-only bearer token for the blog account (`/v1/workouts`, `/mcp`); without it only interactive sessions work |
| `VITALOOP_BLOG_ACCOUNT_ID` | Account id exposed by the blog route |
| `BLOG_HEALTH_ACCOUNT_ID` | Sets the account projected by `/blog/*` directly; takes precedence over the previous one |
| `EPHEMERIS_UPSTREAM` | Root of the hourly RINEX site used to compute ephemeris |
| `EPHEMERIS_RINEX` | Local RINEX file path; when set, upstream download is skipped |

Data layout:

```
<data>/accounts/<account_id>/health.json          full snapshot (may be gzip)
<data>/accounts/<account_id>/health.patches.jsonl  delta patches
<data>/accounts/<account_id>/workouts.json         workout table
<data>/accounts/<id>.json                          account record
<data>/accounts/sessions                          token\taccount_id\texp_unix
```

## Blog sleep projection

`GET /blog/sleep?days=30` (`days` 1–365, default 30) returns the account's last N
nights, using the same rules as the app home screen: nights are grouped by
wake-up date (UTC+8), total sleep comes from the official `anchor_min` values,
`sleep_bed` supplies bed / wake time, score and efficiency, and `sleep_stages`
is only used to draw the latest night's stage bands.

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

Durations are seconds (`stages` timestamps are milliseconds). `data` is sorted
newest first; `detail` only covers the latest night and is
`{"has_data": false}` when there is nothing to show.

## MCP

`POST /mcp` with `Authorization: Bearer <session>`. One JSON-RPC response per
request, no SSE; protocol versions `2024-11-05` / `2025-03-26` / `2025-06-18`
are supported.

```bash
curl -sS -X POST http://127.0.0.1:8793/mcp \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'

curl -sS -X POST http://127.0.0.1:8793/mcp \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call",
       "params":{"name":"blood_pressure","arguments":{"from_ms":1700000000000,"limit":20}}}'
```

Tools (all read-only):

| Tool | Contents |
|---|---|
| `health_summary` | Last N days overview (weight/blood pressure/glucose/sleep/steps/resting HR/HRV/SpO2/stress/temperature) |
| `blood_pressure` | Blood pressure records |
| `blood_glucose` | Blood glucose records |
| `weight` | Weight and body composition |
| `temperature` | Wrist and core temperature |
| `metric_points` | Generic metric series (`skin_temp`, `body_temp`, `resting_hr`, …) |
| `hrv` | HRV records |
| `stress` | Daily stress |
| `spo2` | Daily SpO2 |
| `heart_rate` | Heart rate samples (last 24 hours by default) |
| `sleep` | Sleep sessions, time in bed/score, and stages |
| `daily_metrics` | Daily summaries |
| `workouts` | Workout records |
| `query_table` | Any read-only table on the allowlist, including `ecg_records` / `ecg_samples` |

Conventions: time arguments are Unix milliseconds, daily summaries use
`date_day` (epoch days); `limit` defaults to 200 and caps at 2000; values are
returned without unit conversion. Results include both `structuredContent` and
an equivalent `content[0].text`.

## Security

- The repository contains no deployment domain, host address, or credential;
  the public URL comes from the reverse proxy `Host` header or
  `PUBLIC_BASE_URL`.
- Passwords are stored only as salted iterated hashes; session tokens are 32
  random bytes persisted in the data directory — mind the directory
  permissions.
- `query_table` is read-only and allowlisted; unknown table names are rejected.

## License

AGPL-3.0, see [LICENSE](LICENSE).
