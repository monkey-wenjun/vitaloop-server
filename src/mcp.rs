//! MCP（Model Context Protocol）Streamable HTTP 只读接口。
//!
//! 传输：POST /mcp，JSON-RPC 2.0，一次请求一条消息，响应 application/json。
//! 鉴权：复用账号 Bearer 会话（与 /v1/account/me 同一条 sessions 表），无 token 401。
//! 数据：全部来自该账号的 health 快照（health.json + patches），只读，不写任何文件。
//!
//! 支持的 JSON-RPC 方法：initialize / notifications/initialized / ping /
//! tools/list / tools/call。工具清单见 `tool_definitions`。

use crate::account;
use crate::json::{self, Json};
use crate::Request;
use std::net::TcpStream;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const DEFAULT_PROTOCOL: &str = "2025-06-18";
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2000;
const DAY_MS: f64 = 86_400_000.0;

/// 行内时间列的识别顺序；`ts` 是秒，`date_day` 是 epoch 天。
const TIME_KEYS: [&str; 6] = [
    "timestamp_ms",
    "ts_ms",
    "start_ms",
    "session_start_ms",
    "date_day",
    "ts",
];

/// query_table 的只读白名单：与 Vitaloop cloud.rs 的 TABLES 一致。
const HEALTH_TABLES: [&str; 32] = [
    "daily_metrics",
    "hrv_records",
    "sleep_hrv_records",
    "sleep_sessions",
    "heart_rate_points",
    "step_samples",
    "weight_records",
    "workouts",
    "workout_tombstones",
    "profile",
    "settings",
    "devices",
    "metric_points",
    "blood_pressure_records",
    "blood_glucose_records",
    "arrhythmia_records",
    "ecg_records",
    "ecg_samples",
    "sleep_stages",
    "sleep_bed",
    "emotion_records",
    "equipment",
    "strength_exercises",
    "strength_templates",
    "strength_template_exercises",
    "strength_workout_sets",
    "apnea_records",
    "hourly_status",
    "daily_rings",
    "saved_routes",
    "saved_route_tombstones",
    "health_record_tombstones",
];

pub fn handle(stream: &mut TcpStream, req: &Request, root: &Path) -> std::io::Result<()> {
    if req.method != "POST" {
        return crate::respond(stream, 405, "text/plain", b"method", None);
    }
    let (code, body) = process(req, root);
    crate::respond(stream, code, "application/json", &body, None)
}

fn process(req: &Request, root: &Path) -> (u16, Vec<u8>) {
    let Some(text) = std::str::from_utf8(&req.body).ok() else {
        return (400, error_body(None, -32700, "parse error: invalid utf-8"));
    };
    let Ok(message) = json::parse(text) else {
        return (400, error_body(None, -32700, "parse error"));
    };
    if message.as_array().is_some() {
        return (400, error_body(None, -32600, "batch not supported"));
    }
    if message.as_object().is_none() {
        return (400, error_body(None, -32600, "invalid request"));
    }
    let id = message.get("id").cloned();
    let Some(method) = message.get("method").and_then(Json::as_str) else {
        return (400, error_body(id, -32600, "missing method"));
    };

    let Some(token) = account::bearer(req) else {
        return (401, error_body(id, -32001, "missing bearer"));
    };
    let Some(account_id) = account::session_account(root, token) else {
        return (401, error_body(id, -32001, "bad session"));
    };

    let Some(id) = id else {
        // 通知没有 id，不回 JSON-RPC 响应。
        return (202, Vec::new());
    };
    let params = message
        .get("params")
        .cloned()
        .unwrap_or(Json::Object(Vec::new()));

    match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Json::as_str)
                .unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&requested) {
                requested
            } else {
                DEFAULT_PROTOCOL
            };
            let result = Json::Object(vec![
                (k("protocolVersion"), s(version)),
                (
                    k("capabilities"),
                    Json::Object(vec![(k("tools"), Json::Object(Vec::new()))]),
                ),
                (
                    k("serverInfo"),
                    Json::Object(vec![
                        (k("name"), s("vitaloop-health")),
                        (k("version"), s(env!("CARGO_PKG_VERSION"))),
                    ]),
                ),
                (k("instructions"), s(INSTRUCTIONS)),
            ]);
            (200, ok_body(Some(id), result))
        }
        "ping" => (200, ok_body(Some(id), Json::Object(Vec::new()))),
        "tools/list" => {
            let result = Json::Object(vec![(k("tools"), Json::Array(tool_definitions()))]);
            (200, ok_body(Some(id), result))
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Json::as_str) else {
                return (400, error_body(Some(id), -32602, "missing tool name"));
            };
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(Json::Object(Vec::new()));
            if args.as_object().is_none() {
                return (
                    400,
                    error_body(Some(id), -32602, "arguments must be an object"),
                );
            }
            let result = match call_tool(root, &account_id, name, &args) {
                Ok(payload) => tool_ok(payload),
                Err(message) => tool_error(&message),
            };
            (200, ok_body(Some(id), result))
        }
        other => (
            200,
            error_body(Some(id), -32601, &format!("method not found: {other}")),
        ),
    }
}

const INSTRUCTIONS: &str = "Vitaloop 健康数据只读服务。时间参数均为 Unix 毫秒；\
当天汇总表用 date_day（epoch 天）。返回的数值不做单位换算，字段名即原表列名。";

fn call_tool(root: &Path, account_id: &str, name: &str, args: &Json) -> Result<Json, String> {
    match name {
        "health_summary" => summary(root, account_id, args),
        "blood_pressure" => {
            table_tool(root, account_id, "blood_pressure_records", args, None, None)
        }
        "blood_glucose" => table_tool(root, account_id, "blood_glucose_records", args, None, None),
        "weight" => table_tool(root, account_id, "weight_records", args, None, None),
        "hrv" => table_tool(root, account_id, "hrv_records", args, None, None),
        "sleep" => sleep_tool(root, account_id, args),
        "daily_metrics" => table_tool(root, account_id, "daily_metrics", args, None, None),
        "stress" => table_tool(
            root,
            account_id,
            "daily_metrics",
            args,
            None,
            Some(&["date_day", "stress"]),
        ),
        "spo2" => table_tool(
            root,
            account_id,
            "daily_metrics",
            args,
            None,
            Some(&["date_day", "spo2"]),
        ),
        "heart_rate" => table_tool(
            root,
            account_id,
            "heart_rate_points",
            args,
            Some(now_ms() - DAY_MS),
            None,
        ),
        "temperature" => metric_tool(root, account_id, args, &["skin_temp", "body_temp"]),
        "metric_points" => metric_tool(root, account_id, args, &[]),
        "workouts" => workouts_tool(root, account_id, args),
        "query_table" => query_table_tool(root, account_id, args),
        other => Err(format!("unknown tool: {other}")),
    }
}

// ---------------- 工具实现 ----------------

fn summary(root: &Path, account_id: &str, args: &Json) -> Result<Json, String> {
    let days = args
        .get("days")
        .and_then(Json::as_f64)
        .unwrap_or(7.0)
        .clamp(1.0, 3650.0);
    let now = now_ms();
    let from = now - days * DAY_MS;

    let names = [
        "weight_records",
        "blood_pressure_records",
        "blood_glucose_records",
        "sleep_sessions",
        "sleep_bed",
        "daily_metrics",
        "hrv_records",
        "metric_points",
    ];
    let snapshot = load(root, account_id, &names);

    let weights = window_of(&snapshot, "weight_records", from, now);
    let pressures = window_of(&snapshot, "blood_pressure_records", from, now);
    let glucoses = window_of(&snapshot, "blood_glucose_records", from, now);
    let beds = window_of(&snapshot, "sleep_bed", from, now);
    let daily = window_of(&snapshot, "daily_metrics", from, now);
    let hrvs = window_of(&snapshot, "hrv_records", from, now);
    let metrics = window_of(&snapshot, "metric_points", from, now);

    let weight = weights.first().map(|row| {
        object(&[
            ("weight_kg", opt(row, "weight_kg")),
            ("body_fat_percent", opt(row, "body_fat_percent")),
            ("timestamp_ms", opt(row, "timestamp_ms")),
        ])
    });

    let blood_pressure = object(&[
        (
            "latest",
            pressures
                .first()
                .map(|row| {
                    object(&[
                        ("systolic", opt(row, "systolic")),
                        ("diastolic", opt(row, "diastolic")),
                        ("pulse", opt(row, "pulse")),
                        ("timestamp_ms", opt(row, "timestamp_ms")),
                    ])
                })
                .unwrap_or(Json::Null),
        ),
        ("avg_systolic", number(avg(&pressures, "systolic"))),
        ("avg_diastolic", number(avg(&pressures, "diastolic"))),
        ("samples", Json::Number(pressures.len() as f64)),
    ]);

    let blood_glucose = object(&[
        (
            "latest",
            glucoses
                .first()
                .map(|row| {
                    object(&[
                        ("value", opt(row, "value")),
                        ("measure_type", opt(row, "measure_type")),
                        ("timestamp_ms", opt(row, "timestamp_ms")),
                    ])
                })
                .unwrap_or(Json::Null),
        ),
        ("avg_value", number(avg(&glucoses, "value"))),
        ("samples", Json::Number(glucoses.len() as f64)),
    ]);

    let sleep = object(&[
        ("avg_sleep_minutes", number(avg(&daily, "sleep_minutes"))),
        (
            "avg_deep_minutes",
            number(avg(&daily, "deep_sleep_minutes")),
        ),
        ("avg_rem_minutes", number(avg(&daily, "rem_sleep_minutes"))),
        ("avg_awake_minutes", number(avg(&daily, "awake_minutes"))),
        (
            "latest_watch_score",
            beds.first()
                .map_or(Json::Null, |row| opt(row, "watch_score")),
        ),
        (
            "latest_watch_efficiency",
            beds.first()
                .map_or(Json::Null, |row| opt(row, "watch_efficiency")),
        ),
    ]);

    let activity = object(&[
        ("avg_steps", number(avg(&daily, "steps"))),
        (
            "avg_active_calories",
            number(avg(&daily, "active_calories")),
        ),
        ("avg_distance_m", number(avg(&daily, "distance_m"))),
        ("avg_active_minutes", number(avg(&daily, "active_minutes"))),
    ]);

    let heart = object(&[
        ("latest_resting_hr", first_value(&daily, "resting_hr")),
        ("latest_hrv_ms", first_value(&hrvs, "hrv_ms")),
        ("avg_hrv_ms", number(avg(&hrvs, "hrv_ms"))),
    ]);

    let vitals = object(&[
        ("latest_spo2", first_value(&daily, "spo2")),
        ("latest_stress", first_value(&daily, "stress")),
        ("latest_skin_temp_c", metric_values(&metrics, "skin_temp")),
        ("latest_body_temp_c", metric_values(&metrics, "body_temp")),
    ]);

    Ok(object(&[
        ("days", Json::Number(days)),
        ("from_ms", Json::Number(from)),
        ("to_ms", Json::Number(now)),
        ("weight", weight.unwrap_or(Json::Null)),
        ("blood_pressure", blood_pressure),
        ("blood_glucose", blood_glucose),
        ("sleep", sleep),
        ("activity", activity),
        ("heart", heart),
        ("vitals", vitals),
    ]))
}

fn table_tool(
    root: &Path,
    account_id: &str,
    table: &str,
    args: &Json,
    default_from: Option<f64>,
    keep: Option<&[&str]>,
) -> Result<Json, String> {
    let from = arg_ms(args, "from_ms").or(default_from);
    let to = arg_ms(args, "to_ms");
    let limit = limit_arg(args);
    let snapshot = load(root, account_id, &[table]);
    let (rows, has_time) = table_rows(&snapshot, table);
    let mut rows = filtered(rows, has_time, from, to, limit);
    if let Some(keep) = keep {
        rows = rows
            .iter()
            .map(|row| project(row, keep))
            .filter(|row| {
                // 只保留该列的读数为真的行（stress / spo2 没测的那天不出现在结果里）。
                keep.iter().any(|key| {
                    row.get(key)
                        .map(|value| !matches!(value, Json::Null))
                        .unwrap_or(false)
                })
            })
            .collect();
    }
    Ok(window_reply(table, from, to, limit, rows))
}

fn metric_tool(
    root: &Path,
    account_id: &str,
    args: &Json,
    allowed: &[&str],
) -> Result<Json, String> {
    let wanted = args.get("metric").and_then(Json::as_str);
    if let Some(metric) = wanted {
        if !allowed.is_empty() && !allowed.contains(&metric) {
            return Err(format!(
                "unsupported metric {metric}; expected one of {}",
                allowed.join(", ")
            ));
        }
    }
    let from = arg_ms(args, "from_ms");
    let to = arg_ms(args, "to_ms");
    let limit = limit_arg(args);
    let snapshot = load(root, account_id, &["metric_points"]);
    let (rows, has_time) = table_rows(&snapshot, "metric_points");
    let rows = rows
        .into_iter()
        .filter(|row| match wanted {
            Some(metric) => row.get("metric").and_then(Json::as_str) == Some(metric),
            None => {
                allowed.is_empty()
                    || row
                        .get("metric")
                        .and_then(Json::as_str)
                        .map(|m| allowed.contains(&m))
                        .unwrap_or(false)
            }
        })
        .collect();
    let rows = filtered(rows, has_time, from, to, limit);
    Ok(window_reply("metric_points", from, to, limit, rows))
}

fn sleep_tool(root: &Path, account_id: &str, args: &Json) -> Result<Json, String> {
    let from = arg_ms(args, "from_ms");
    let to = arg_ms(args, "to_ms");
    let limit = limit_arg(args);
    let names = ["sleep_sessions", "sleep_bed", "sleep_stages"];
    let snapshot = load(root, account_id, &names);
    let pick = |table: &str| {
        let (rows, has_time) = table_rows(&snapshot, table);
        let rows = filtered(rows, has_time, from, to, limit);
        (Json::Array(rows.clone()), Json::Number(rows.len() as f64))
    };
    let (sessions, session_count) = pick("sleep_sessions");
    let (bed, bed_count) = pick("sleep_bed");
    let (stages, stage_count) = pick("sleep_stages");
    Ok(object(&[
        ("from_ms", number(from)),
        ("to_ms", number(to)),
        ("limit", Json::Number(limit as f64)),
        ("session_count", session_count),
        ("sessions", sessions),
        ("bed_count", bed_count),
        ("bed", bed),
        ("stage_count", stage_count),
        ("stages", stages),
    ]))
}

fn workouts_tool(root: &Path, account_id: &str, args: &Json) -> Result<Json, String> {
    let from = arg_ms(args, "from_ms");
    let to = arg_ms(args, "to_ms");
    let limit = limit_arg(args);
    let rows: Vec<Json> = account::load_workouts(root, account_id)
        .unwrap_or_default()
        .iter()
        .cloned()
        .collect();
    let has_time = rows.iter().any(|row| time_of(row).is_some());
    let rows = filtered(rows, has_time, from, to, limit);
    Ok(window_reply("workouts", from, to, limit, rows))
}

fn query_table_tool(root: &Path, account_id: &str, args: &Json) -> Result<Json, String> {
    let Some(table) = args.get("table").and_then(Json::as_str) else {
        return Err("missing table".to_string());
    };
    if !HEALTH_TABLES.contains(&table) {
        return Err(format!("table {table} is not readable"));
    }
    table_tool(root, account_id, table, args, None, None)
}

// ---------------- 快照与行处理 ----------------

fn load(root: &Path, account_id: &str, names: &[&str]) -> Json {
    account::merged_health_tables(root, account_id, names)
        .unwrap_or_else(|| object(&[("tables", Json::Object(Vec::new()))]))
}

/// 把 `{columns, rows}` 展开成对象数组；同时报告该表是否有时间列。
fn table_rows(snapshot: &Json, table: &str) -> (Vec<Json>, bool) {
    let Some(block) = snapshot.get("tables").and_then(|tables| tables.get(table)) else {
        return (Vec::new(), false);
    };
    let columns: Vec<String> = block
        .get("columns")
        .and_then(Json::as_array)
        .map(|columns| {
            columns
                .iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let has_time = columns.iter().any(|name| is_time_key(name));
    let mut out = Vec::new();
    if let Some(rows) = block.get("rows").and_then(Json::as_array) {
        for row in rows {
            let Some(values) = row.as_array() else {
                continue;
            };
            if values.len() < columns.len() {
                continue;
            }
            let fields = columns
                .iter()
                .zip(values.iter())
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            out.push(Json::Object(fields));
        }
    }
    (out, has_time)
}

fn window_of(snapshot: &Json, table: &str, from: f64, to: f64) -> Vec<Json> {
    let (rows, has_time) = table_rows(snapshot, table);
    filtered(rows, has_time, Some(from), Some(to), MAX_LIMIT)
}

fn filtered(
    mut rows: Vec<Json>,
    has_time: bool,
    from: Option<f64>,
    to: Option<f64>,
    limit: usize,
) -> Vec<Json> {
    if has_time {
        rows.retain(|row| match time_of(row) {
            Some(ts) => from.map_or(true, |from| ts >= from) && to.map_or(true, |to| ts <= to),
            // 有时间列但该行是 null：无法判断窗口，保留，避免静默丢数据。
            None => true,
        });
        rows.sort_by(|a, b| {
            let ta = time_of(a).unwrap_or(f64::NEG_INFINITY);
            let tb = time_of(b).unwrap_or(f64::NEG_INFINITY);
            tb.partial_cmp(&ta).unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    rows.truncate(limit);
    rows
}

fn time_of(row: &Json) -> Option<f64> {
    for key in ["timestamp_ms", "ts_ms", "start_ms", "session_start_ms"] {
        if let Some(value) = row.get(key).and_then(Json::as_f64) {
            return Some(value);
        }
    }
    if let Some(day) = row.get("date_day").and_then(Json::as_f64) {
        let at_sec = row.get("at_sec").and_then(Json::as_f64).unwrap_or(0.0);
        return Some(day * DAY_MS + at_sec * 1000.0);
    }
    row.get("ts").and_then(Json::as_f64).map(|sec| sec * 1000.0)
}

fn is_time_key(name: &str) -> bool {
    TIME_KEYS.contains(&name) || name == "at_sec"
}

fn project(row: &Json, keep: &[&str]) -> Json {
    let fields = keep
        .iter()
        .filter_map(|key| {
            row.get(key)
                .map(|value| ((*key).to_string(), value.clone()))
        })
        .collect();
    Json::Object(fields)
}

fn avg(rows: &[Json], key: &str) -> Option<f64> {
    let values: Vec<f64> = rows
        .iter()
        .filter_map(|row| row.get(key).and_then(Json::as_f64))
        .collect();
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

fn first_value(rows: &[Json], key: &str) -> Json {
    rows.iter()
        .find_map(|row| {
            row.get(key)
                .filter(|value| !matches!(value, Json::Null))
                .cloned()
        })
        .unwrap_or(Json::Null)
}

fn metric_values(rows: &[Json], metric: &str) -> Json {
    rows.iter()
        .find(|row| row.get("metric").and_then(Json::as_str) == Some(metric))
        .map(|row| object(&[("value", opt(row, "value")), ("ts_ms", opt(row, "ts_ms"))]))
        .unwrap_or(Json::Null)
}

fn window_reply(
    table: &str,
    from: Option<f64>,
    to: Option<f64>,
    limit: usize,
    rows: Vec<Json>,
) -> Json {
    object(&[
        ("table", s(table)),
        ("count", Json::Number(rows.len() as f64)),
        ("from_ms", number(from)),
        ("to_ms", number(to)),
        ("limit", Json::Number(limit as f64)),
        ("rows", Json::Array(rows)),
    ])
}

// ---------------- 参数 ----------------

fn limit_arg(args: &Json) -> usize {
    args.get("limit")
        .and_then(Json::as_f64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT)
}

fn arg_ms(args: &Json, key: &str) -> Option<f64> {
    args.get(key).and_then(Json::as_f64)
}

// ---------------- 工具清单 ----------------

fn tool_definitions() -> Vec<Json> {
    vec![
        tool(
            "health_summary",
            "近 N 天健康总览：最新体重/血压/血糖、睡眠与运动均值、静息心率、HRV、血氧、压力、体温。",
            object_schema(vec![(
                k("days"),
                int_prop("统计天数，1–3650，默认 7"),
            )]),
        ),
        tool(
            "blood_pressure",
            "血压记录：收缩压 systolic、舒张压 diastolic、脉搏 pulse。按时间倒序。",
            window_schema(true, false),
        ),
        tool(
            "blood_glucose",
            "血糖记录：value 与 measure_type（空腹/餐后/随机等）。按时间倒序。",
            window_schema(true, false),
        ),
        tool(
            "weight",
            "体重记录：weight_kg 及体脂、肌肉、水分等成分字段。按时间倒序。",
            window_schema(true, false),
        ),
        tool(
            "temperature",
            "体温记录（metric_points 中的 skin_temp 腕温与 body_temp 核心体温）。腕温只有 28–34°C，不能当核心体温。",
            window_schema(true, true),
        ),
        tool(
            "metric_points",
            "通用指标时间序列（metric_points 表）：metric 如 skin_temp、body_temp、resting_hr。",
            window_schema(true, true),
        ),
        tool(
            "hrv",
            "HRV 记录（hrv_records）：hrv_ms、心率、压力、质量。按时间倒序。",
            window_schema(true, false),
        ),
        tool(
            "stress",
            "每日压力（daily_metrics.stress），date_day 为 epoch 天。",
            window_schema(true, false),
        ),
        tool(
            "spo2",
            "每日血氧（daily_metrics.spo2），date_day 为 epoch 天。",
            window_schema(true, false),
        ),
        tool(
            "heart_rate",
            "心率采样点（heart_rate_points）。默认最近 24 小时，按时间倒序。",
            window_schema(true, false),
        ),
        tool(
            "sleep",
            "睡眠会话、手表卧床/评分与睡眠分期。返回 sessions / bed / stages 三段。",
            window_schema(true, false),
        ),
        tool(
            "daily_metrics",
            "每日汇总：步数、热量、距离、静息心率、睡眠时长、HRV、压力、血氧、腕温。",
            window_schema(true, false),
        ),
        tool(
            "workouts",
            "运动记录，按开始时间倒序。",
            window_schema(true, false),
        ),
        tool(
            "query_table",
            "只读查询白名单内的任意健康表；table 必填。用于上面工具没覆盖的表。",
            object_schema(vec![
                (k("table"), str_prop("表名，必须在只读白名单内")),
                (k("from_ms"), num_prop("起始时间，Unix 毫秒（含）")),
                (k("to_ms"), num_prop("结束时间，Unix 毫秒（含）")),
                (k("limit"), int_prop("最多返回行数，默认 200，上限 2000")),
            ]),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Json) -> Json {
    object(&[
        ("name", s(name)),
        ("description", s(description)),
        ("inputSchema", input_schema),
        (
            "annotations",
            object(&[
                ("readOnlyHint", Json::Bool(true)),
                ("destructiveHint", Json::Bool(false)),
                ("idempotentHint", Json::Bool(true)),
                ("openWorldHint", Json::Bool(false)),
            ]),
        ),
    ])
}

fn window_schema(with_limit: bool, with_metric: bool) -> Json {
    let mut props = vec![
        (k("from_ms"), num_prop("起始时间，Unix 毫秒（含）")),
        (k("to_ms"), num_prop("结束时间，Unix 毫秒（含）")),
    ];
    if with_limit {
        props.push((k("limit"), int_prop("最多返回行数，默认 200，上限 2000")));
    }
    if with_metric {
        props.push((
            k("metric"),
            str_prop("只取某一个指标，如 skin_temp / body_temp"),
        ));
    }
    object_schema(props)
}

fn object_schema(props: Vec<(String, Json)>) -> Json {
    object(&[
        ("type", s("object")),
        ("properties", Json::Object(props)),
        ("additionalProperties", Json::Bool(false)),
    ])
}

fn num_prop(description: &str) -> Json {
    object(&[("type", s("number")), ("description", s(description))])
}

fn int_prop(description: &str) -> Json {
    object(&[
        ("type", s("integer")),
        ("description", s(description)),
        ("minimum", Json::Number(1.0)),
    ])
}

fn str_prop(description: &str) -> Json {
    object(&[("type", s("string")), ("description", s(description))])
}

// ---------------- JSON-RPC 信封 ----------------

fn tool_ok(payload: Json) -> Json {
    let text = payload.render();
    object(&[
        (
            "content",
            Json::Array(vec![object(&[
                ("type", s("text")),
                ("text", Json::String(text)),
            ])]),
        ),
        ("structuredContent", payload),
        ("isError", Json::Bool(false)),
    ])
}

fn tool_error(message: &str) -> Json {
    object(&[
        (
            "content",
            Json::Array(vec![object(&[("type", s("text")), ("text", s(message))])]),
        ),
        ("isError", Json::Bool(true)),
    ])
}

fn ok_body(id: Option<Json>, result: Json) -> Vec<u8> {
    object(&[
        ("jsonrpc", s("2.0")),
        ("id", id.unwrap_or(Json::Null)),
        ("result", result),
    ])
    .render()
    .into_bytes()
}

fn error_body(id: Option<Json>, code: i64, message: &str) -> Vec<u8> {
    object(&[
        ("jsonrpc", s("2.0")),
        ("id", id.unwrap_or(Json::Null)),
        (
            "error",
            object(&[("code", Json::Number(code as f64)), ("message", s(message))]),
        ),
    ])
    .render()
    .into_bytes()
}

// ---------------- 小工具 ----------------

fn k(key: &str) -> String {
    key.to_string()
}

fn s(value: &str) -> Json {
    Json::String(value.to_string())
}

fn object(fields: &[(&str, Json)]) -> Json {
    Json::Object(
        fields
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect(),
    )
}

fn opt(row: &Json, key: &str) -> Json {
    row.get(key).cloned().unwrap_or(Json::Null)
}

fn number(value: Option<f64>) -> Json {
    value.map(Json::Number).unwrap_or(Json::Null)
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    const SNAPSHOT: &str = r#"{"version":1,"exported_ms":0,"tables":{
 "blood_pressure_records":{"columns":["id","timestamp_ms","systolic","diastolic","pulse"],"rows":[["bp1",1700000001000,120,80,60],["bp2",1700000002000,130,85,65],["bp3",1700000003000,125,82,70]]},
 "weight_records":{"columns":["id","timestamp_ms","weight_kg","body_fat_percent"],"rows":[["w1",1700000001000,70.5,18.0],["w2",1700000002000,70.0,17.8]]},
 "blood_glucose_records":{"columns":["id","timestamp_ms","value","measure_type"],"rows":[["g1",1700000001000,5.6,"fasting"]]},
 "daily_metrics":{"columns":["date_day","steps","active_calories","distance_m","active_minutes","resting_hr","sleep_minutes","deep_sleep_minutes","rem_sleep_minutes","awake_minutes","hrv_ms","stress","spo2","skin_temp_c","updated_ms"],"rows":[[20000,9000,400,8000,60,58,420,80,90,20,45,30,97,32.5,0],[20001,8000,350,7000,50,59,400,75,85,25,47,32,96,32.8,0]]},
 "sleep_sessions":{"columns":["id","start_ms","end_ms","deep_min","light_min","rem_min","awake_min","anchor_min","source"],"rows":[[1,1700000000000,1700009000000,80,300,90,10,470,"watch"]]},
 "sleep_bed":{"columns":["session_start_ms","bed_ms","fall_asleep_ms","wake_ms","duration_min","watch_score","watch_efficiency","watch_wake_count"],"rows":[[1700000000000,500,900,2000,470,88,92,2]]},
 "sleep_stages":{"columns":["start_ms","end_ms","stage","source"],"rows":[[1699999999000,1700000000000,"awake","watch"],[1700000000000,1700002000000,"light","watch"]]},
 "hrv_records":{"columns":["id","date_day","at_sec","hrv_ms","heart_rate","stress","quality","source"],"rows":[[1,20000,3600,45,60,30,"good","watch"],[2,20001,7200,47,62,32,"good","watch"]]},
 "metric_points":{"columns":["metric","ts_ms","value","source"],"rows":[["skin_temp",1700000001000,32.5,"watch"],["body_temp",1700000002000,36.6,"watch"],["skin_temp",1700000003000,32.8,"watch"]]},
 "heart_rate_points":{"columns":["ts_ms","bpm","synced"],"rows":[[1000,60,1],[2000,65,1]]},
 "workouts":{"columns":["id","sport","start_ms","distance_m"],"rows":[["w-old","outdoor_run",1000,5.0]]}
}}"#;

    struct Fixture {
        root: PathBuf,
        token: String,
    }

    fn dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vitaloop-server-mcp-test-{}-{}-{}",
            std::process::id(),
            now_ms() as u64,
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    impl Fixture {
        fn new() -> Fixture {
            let root = dir();
            let register = Request {
                method: "POST".into(),
                path: "/v1/account/register".into(),
                headers: HashMap::new(),
                body: br#"{"email":"mcp@example.com","password":"test-password-123"}"#.to_vec(),
            };
            let (code, _, body) = account::route(&register, &root).unwrap();
            assert_eq!(code, 201);
            let parsed = json::parse(std::str::from_utf8(&body).unwrap()).unwrap();
            let token = parsed
                .get("token")
                .and_then(Json::as_str)
                .unwrap()
                .to_string();
            let fixture = Fixture { root, token };
            fixture.put_health(SNAPSHOT);
            fixture
        }

        fn headers(&self) -> HashMap<String, String> {
            let mut headers = HashMap::new();
            headers.insert("authorization".into(), format!("Bearer {}", self.token));
            headers
        }

        fn put_health(&self, snapshot: &str) {
            let req = Request {
                method: "PUT".into(),
                path: "/v1/health/data".into(),
                headers: self.headers(),
                body: snapshot.as_bytes().to_vec(),
            };
            assert_eq!(account::route(&req, &self.root).unwrap().0, 200);
        }

        fn rpc(&self, body: &str) -> (u16, Json) {
            self.rpc_with(self.headers(), body)
        }

        fn rpc_with(&self, headers: HashMap<String, String>, body: &str) -> (u16, Json) {
            let req = Request {
                method: "POST".into(),
                path: "/mcp".into(),
                headers,
                body: body.as_bytes().to_vec(),
            };
            let (code, bytes) = process(&req, &self.root);
            let parsed = if bytes.is_empty() {
                Json::Null
            } else {
                json::parse(std::str::from_utf8(&bytes).unwrap()).unwrap()
            };
            let _ = &self.root;
            (code, parsed)
        }

        fn call(&self, name: &str, args: &str) -> Json {
            let body = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
            );
            let (code, resp) = self.rpc(&body);
            assert_eq!(code, 200);
            resp.get("result").cloned().expect("tool result")
        }
    }

    fn structured(result: &Json) -> &Json {
        result.get("structuredContent").expect("structured content")
    }

    fn is_error(result: &Json) -> bool {
        matches!(result.get("isError"), Some(Json::Bool(true)))
    }

    fn rows(result: &Json) -> &[Json] {
        structured(result)
            .get("rows")
            .and_then(Json::as_array)
            .expect("rows")
    }

    #[test]
    fn initialize_negotiates_protocol_version() {
        let f = Fixture::new();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
        let (code, resp) = f.rpc(body);
        assert_eq!(code, 200);
        let result = resp.get("result").unwrap();
        assert_eq!(
            result.get("protocolVersion").and_then(Json::as_str),
            Some("2024-11-05")
        );
        assert_eq!(
            result
                .get("serverInfo")
                .and_then(|v| v.get("name"))
                .and_then(Json::as_str),
            Some("vitaloop-health")
        );

        let body = r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2030-01-01"}}"#;
        let (_, resp) = f.rpc(body);
        assert_eq!(
            resp.get("result")
                .and_then(|v| v.get("protocolVersion"))
                .and_then(Json::as_str),
            Some("2025-06-18")
        );
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn ping_unknown_method_and_notifications() {
        let f = Fixture::new();
        let (code, resp) = f.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        assert_eq!(code, 200);
        assert!(resp.get("result").is_some());

        let (code, resp) = f.rpc(r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#);
        assert_eq!(code, 200);
        assert_eq!(
            resp.get("error")
                .and_then(|v| v.get("code"))
                .and_then(Json::as_f64),
            Some(-32601.0)
        );

        let (code, resp) = f.rpc(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!(code, 202);
        assert!(matches!(resp, Json::Null));
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn mcp_requires_valid_bearer_session() {
        let f = Fixture::new();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let (code, resp) = f.rpc_with(HashMap::new(), body);
        assert_eq!(code, 401);
        assert_eq!(
            resp.get("error")
                .and_then(|v| v.get("message"))
                .and_then(Json::as_str),
            Some("missing bearer")
        );

        let mut headers = HashMap::new();
        headers.insert("authorization".into(), "Bearer nope".into());
        let (code, resp) = f.rpc_with(headers, body);
        assert_eq!(code, 401);
        assert_eq!(
            resp.get("error")
                .and_then(|v| v.get("message"))
                .and_then(Json::as_str),
            Some("bad session")
        );
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn tools_list_exposes_only_read_only_tools() {
        let f = Fixture::new();
        let (_, resp) = f.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let tools = resp
            .get("result")
            .and_then(|v| v.get("tools"))
            .and_then(Json::as_array)
            .unwrap();
        assert_eq!(tools.len(), 14);
        for expected in [
            "health_summary",
            "blood_pressure",
            "blood_glucose",
            "weight",
            "temperature",
            "metric_points",
            "hrv",
            "stress",
            "spo2",
            "heart_rate",
            "sleep",
            "daily_metrics",
            "workouts",
            "query_table",
        ] {
            assert!(
                tools
                    .iter()
                    .any(|t| t.get("name").and_then(Json::as_str) == Some(expected)),
                "missing tool {expected}"
            );
        }
        for t in tools {
            assert!(matches!(
                t.get("annotations").and_then(|a| a.get("readOnlyHint")),
                Some(Json::Bool(true))
            ));
        }
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn blood_pressure_honours_window_and_limit() {
        let f = Fixture::new();
        let result = f.call(
            "blood_pressure",
            r#"{"from_ms":1700000002000,"to_ms":1700000003000,"limit":1}"#,
        );
        assert!(!is_error(&result));
        let rows = rows(&result);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("systolic").and_then(Json::as_f64), Some(125.0));
        assert_eq!(
            structured(&result).get("count").and_then(Json::as_f64),
            Some(1.0)
        );
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn metric_tools_filter_by_metric_and_reject_unknown_names() {
        let f = Fixture::new();
        let result = f.call("temperature", "{}");
        assert!(!is_error(&result));
        assert_eq!(rows(&result).len(), 3);

        let result = f.call("metric_points", r#"{"metric":"body_temp"}"#);
        assert!(!is_error(&result));
        let rows = rows(&result);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("value").and_then(Json::as_f64), Some(36.6));

        let result = f.call("temperature", r#"{"metric":"blood_pressure"}"#);
        assert!(is_error(&result));
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn spo2_uses_epoch_day_window() {
        let f = Fixture::new();
        let from = 20001.0 * DAY_MS;
        let to = 20002.0 * DAY_MS;
        let args = format!(r#"{{"from_ms":{from},"to_ms":{to}}}"#);
        let result = f.call("spo2", &args);
        let rows = rows(&result);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("date_day").and_then(Json::as_f64),
            Some(20001.0)
        );
        assert_eq!(rows[0].get("spo2").and_then(Json::as_f64), Some(96.0));
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn summary_aggregates_recent_data() {
        let f = Fixture::new();
        let result = f.call("health_summary", r#"{"days":3650}"#);
        assert!(!is_error(&result));
        let s = structured(&result);

        let weight = s.get("weight").unwrap();
        assert_eq!(weight.get("weight_kg").and_then(Json::as_f64), Some(70.0));

        let bp = s.get("blood_pressure").unwrap();
        assert_eq!(
            bp.get("latest")
                .and_then(|v| v.get("systolic"))
                .and_then(Json::as_f64),
            Some(125.0)
        );
        assert_eq!(bp.get("avg_systolic").and_then(Json::as_f64), Some(125.0));
        let avg_diastolic = bp.get("avg_diastolic").and_then(Json::as_f64).unwrap();
        assert!((avg_diastolic - 247.0 / 3.0).abs() < 1e-9);

        let sleep = s.get("sleep").unwrap();
        assert_eq!(
            sleep.get("avg_sleep_minutes").and_then(Json::as_f64),
            Some(410.0)
        );
        assert_eq!(
            sleep.get("latest_watch_score").and_then(Json::as_f64),
            Some(88.0)
        );

        let heart = s.get("heart").unwrap();
        assert_eq!(
            heart.get("latest_resting_hr").and_then(Json::as_f64),
            Some(59.0)
        );
        assert_eq!(
            heart.get("latest_hrv_ms").and_then(Json::as_f64),
            Some(47.0)
        );

        let vitals = s.get("vitals").unwrap();
        assert_eq!(vitals.get("latest_spo2").and_then(Json::as_f64), Some(96.0));
        assert_eq!(
            vitals.get("latest_stress").and_then(Json::as_f64),
            Some(32.0)
        );
        assert_eq!(
            vitals
                .get("latest_body_temp_c")
                .and_then(|v| v.get("value"))
                .and_then(Json::as_f64),
            Some(36.6)
        );
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn sleep_returns_sessions_bed_and_stages() {
        let f = Fixture::new();
        let result = f.call("sleep", r#"{"limit":10}"#);
        let s = structured(&result);
        assert_eq!(s.get("session_count").and_then(Json::as_f64), Some(1.0));
        assert_eq!(s.get("bed_count").and_then(Json::as_f64), Some(1.0));
        assert_eq!(s.get("stage_count").and_then(Json::as_f64), Some(2.0));
        let bed = s.get("bed").and_then(Json::as_array).unwrap();
        assert_eq!(bed[0].get("watch_score").and_then(Json::as_f64), Some(88.0));
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn heart_rate_defaults_to_last_24h_and_sorts_desc() {
        let f = Fixture::new();
        let result = f.call("heart_rate", "{}");
        assert_eq!(
            rows(&result).len(),
            0,
            "1970 年的采样不该落进默认 24 小时窗口"
        );

        let result = f.call("heart_rate", r#"{"from_ms":0}"#);
        let rows = rows(&result);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("bpm").and_then(Json::as_f64), Some(65.0));
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn workouts_tool_reads_snapshot_table() {
        let f = Fixture::new();
        let result = f.call("workouts", r#"{"from_ms":0}"#);
        let rows = rows(&result);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("sport").and_then(Json::as_str),
            Some("outdoor_run")
        );
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn query_table_rejects_unknown_tables() {
        let f = Fixture::new();
        let result = f.call("query_table", r#"{"table":"accounts"}"#);
        assert!(is_error(&result));

        let result = f.call("query_table", r#"{"table":"weight_records","limit":1}"#);
        assert!(!is_error(&result));
        assert_eq!(rows(&result).len(), 1);
        let _ = fs::remove_dir_all(&f.root);
    }
}
