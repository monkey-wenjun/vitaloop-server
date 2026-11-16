//! Narrow, read-only public projection of body measurements for the blog.
//!
//! The full health snapshot stays private; this endpoint exposes only the
//! weight / blood-pressure / blood-glucose rows the blog cards draw.
//!
//! The app's local database already de-duplicates repeated sync packets
//! (same measurement written several times with millisecond offsets).  The
//! snapshot can still carry those duplicates, so the same rules are applied
//! here: weight within 60s with an identical reading, pressure within 1s with
//! identical readings, glucose within the same minute with an identical value.

use crate::account;
#[cfg(test)]
use crate::json;
use crate::json::Json;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

const DAY_MS: i64 = 86_400_000;

struct CacheEntry {
    key: String,
    snapshot_stamp: FileStamp,
    patches_stamp: FileStamp,
    body: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

static CACHE: OnceLock<Mutex<Option<CacheEntry>>> = OnceLock::new();

/// `GET /blog/body?days=365` body.
pub fn body(root: &Path, days: i64) -> Result<String, String> {
    let account = account_id(root)?;
    let dir = root.join("accounts").join(&account);
    let key = format!("{}|{days}", dir.display());
    let snapshot_stamp = file_stamp(&dir.join("health.json")).ok_or("health snapshot missing")?;
    let patches_stamp = file_stamp(&dir.join("health.patches.jsonl")).unwrap_or(FileStamp {
        modified: None,
        len: 0,
    });
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Some(entry) = cache.lock().unwrap().as_ref() {
        if entry.key == key
            && entry.snapshot_stamp == snapshot_stamp
            && entry.patches_stamp == patches_stamp
        {
            return Ok(entry.body.clone());
        }
    }

    let snapshot = account::merged_health_tables(
        root,
        &account,
        &[
            "weight_records",
            "blood_pressure_records",
            "blood_glucose_records",
        ],
    )
    .ok_or("health snapshot cannot be merged")?;
    let body = project(&snapshot, days)?;
    *cache.lock().unwrap() = Some(CacheEntry {
        key,
        snapshot_stamp,
        patches_stamp,
        body: body.clone(),
    });
    Ok(body)
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path).ok()?;
    Some(FileStamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

fn account_id(root: &Path) -> Result<String, String> {
    if let Ok(id) = std::env::var("BLOG_HEALTH_ACCOUNT_ID") {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    let marker: PathBuf = root.join("blog_account");
    let id = fs::read_to_string(marker)
        .map_err(|_| "blog health account is not configured".to_string())?;
    let id = id.trim();
    if id.is_empty() {
        return Err("blog health account is empty".into());
    }
    Ok(id.to_string())
}

fn table<'a>(snapshot: &'a Json, name: &str) -> (&'a [Json], &'a [Json]) {
    let Some(block) = snapshot.get("tables").and_then(|v| v.get(name)) else {
        return (&[], &[]);
    };
    (
        block.get("columns").and_then(Json::as_array).unwrap_or(&[]),
        block.get("rows").and_then(Json::as_array).unwrap_or(&[]),
    )
}

fn column(columns: &[Json], name: &str) -> Option<usize> {
    columns
        .iter()
        .position(|value| value.as_str() == Some(name))
}

fn cell(row: &[Json], index: Option<usize>) -> Option<&Json> {
    index.and_then(|i| row.get(i))
}

fn number(row: &[Json], index: Option<usize>) -> Option<f64> {
    cell(row, index).and_then(Json::as_f64)
}

fn integer(row: &[Json], index: Option<usize>) -> Option<i64> {
    number(row, index).map(|value| value as i64)
}

/// 读取一张表并按时间升序排好；时间戳缺失或非法的时间列直接丢弃。
fn timed_rows(snapshot: &Json, name: &str) -> (Vec<Json>, Vec<Vec<Json>>) {
    let (columns, rows) = table(snapshot, name);
    let ts_i = column(columns, "timestamp_ms");
    let mut out: Vec<(i64, Vec<Json>)> = rows
        .iter()
        .filter_map(|row| {
            let values = row.as_array()?;
            let ts = integer(values, ts_i)?;
            if ts <= 0 {
                return None;
            }
            Some((ts, values.to_vec()))
        })
        .collect();
    out.sort_by_key(|(ts, _)| *ts);
    (columns.to_vec(), out.into_iter().map(|(_, row)| row).collect())
}

fn copy_missing(keeper: &mut [Json], other: &[Json]) {
    for (slot, value) in keeper.iter_mut().zip(other.iter()) {
        if matches!(slot, Json::Null) && !matches!(value, Json::Null) {
            *slot = value.clone();
        }
    }
}

fn same_reading(left: Option<f64>, right: Option<f64>) -> bool {
    match (left, right) {
        (Some(a), Some(b)) => (a - b).abs() < 0.001,
        (None, None) => true,
        _ => false,
    }
}

fn merge_weight(columns: &[Json], rows: Vec<Vec<Json>>) -> Vec<Vec<Json>> {
    let ts_i = column(columns, "timestamp_ms");
    let weight_i = column(columns, "weight_kg");
    let mut out: Vec<Vec<Json>> = Vec::with_capacity(rows.len());
    let mut start = 0;
    while start < rows.len() {
        let mut end = start + 1;
        while end < rows.len() {
            let gap = integer(&rows[end], ts_i).unwrap_or(0)
                - integer(&rows[start], ts_i).unwrap_or(0);
            let same = same_reading(
                number(&rows[end], weight_i),
                number(&rows[start], weight_i),
            );
            if gap > 60_000 || !same {
                break;
            }
            end += 1;
        }
        let mut keeper = rows[start].clone();
        for row in &rows[start + 1..end] {
            copy_missing(&mut keeper, row);
        }
        out.push(keeper);
        start = end;
    }
    out
}

fn merge_pressure(columns: &[Json], rows: Vec<Vec<Json>>) -> Vec<Vec<Json>> {
    let ts_i = column(columns, "timestamp_ms");
    let sys_i = column(columns, "systolic");
    let dia_i = column(columns, "diastolic");
    let pulse_i = column(columns, "pulse");
    let mut out: Vec<Vec<Json>> = Vec::with_capacity(rows.len());
    let mut start = 0;
    while start < rows.len() {
        let mut end = start + 1;
        while end < rows.len() {
            let gap = integer(&rows[end], ts_i).unwrap_or(0)
                - integer(&rows[start], ts_i).unwrap_or(0);
            let same = same_reading(number(&rows[end], sys_i), number(&rows[start], sys_i))
                && same_reading(number(&rows[end], dia_i), number(&rows[start], dia_i))
                && same_reading(number(&rows[end], pulse_i), number(&rows[start], pulse_i));
            if gap > 1_000 || !same {
                break;
            }
            end += 1;
        }
        let mut keeper = rows[start].clone();
        for row in &rows[start + 1..end] {
            copy_missing(&mut keeper, row);
        }
        out.push(keeper);
        start = end;
    }
    out
}

fn minute_bucket(ts: i64) -> i64 {
    (ts + 30_000).div_euclid(60_000)
}

fn merge_glucose(columns: &[Json], rows: Vec<Vec<Json>>) -> Vec<Vec<Json>> {
    let ts_i = column(columns, "timestamp_ms");
    let value_i = column(columns, "value");
    let mut out: Vec<Vec<Json>> = Vec::with_capacity(rows.len());
    let mut start = 0;
    while start < rows.len() {
        let mut end = start + 1;
        while end < rows.len() {
            let same = minute_bucket(integer(&rows[end], ts_i).unwrap_or(0))
                == minute_bucket(integer(&rows[start], ts_i).unwrap_or(0))
                && same_reading(number(&rows[end], value_i), number(&rows[start], value_i));
            if !same {
                break;
            }
            end += 1;
        }
        let mut keeper = rows[start].clone();
        for row in &rows[start + 1..end] {
            copy_missing(&mut keeper, row);
        }
        out.push(keeper);
        start = end;
    }
    out
}

fn render(columns: &[Json], rows: Vec<Vec<Json>>) -> Json {
    let names: Vec<String> = columns
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_string())
        .collect();
    Json::Array(
        rows.into_iter()
            .map(|row| {
                Json::Object(
                    names
                        .iter()
                        .cloned()
                        .zip(row)
                        .collect::<Vec<(String, Json)>>(),
                )
            })
            .collect(),
    )
}

fn project(snapshot: &Json, days: i64) -> Result<String, String> {
    let days = days.clamp(1, 3_650);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let start = now.saturating_sub(days.saturating_mul(DAY_MS));

    let (weight_columns, weight_rows) = timed_rows(snapshot, "weight_records");
    let weight_i = column(&weight_columns, "weight_kg");
    let weight = merge_weight(&weight_columns, weight_rows)
        .into_iter()
        .filter(|row| {
            number(row, weight_i)
                .map(|value| (20.0..=400.0).contains(&value))
                .unwrap_or(false)
        })
        .collect();

    let (pressure_columns, pressure_rows) = timed_rows(snapshot, "blood_pressure_records");
    let sys_i = column(&pressure_columns, "systolic");
    let dia_i = column(&pressure_columns, "diastolic");
    let pressure = merge_pressure(&pressure_columns, pressure_rows)
        .into_iter()
        .filter(|row| {
            number(row, sys_i)
                .map(|value| (50.0..=250.0).contains(&value))
                .unwrap_or(false)
                && number(row, dia_i)
                    .map(|value| (30.0..=180.0).contains(&value))
                    .unwrap_or(false)
        })
        .collect();

    let (glucose_columns, glucose_rows) = timed_rows(snapshot, "blood_glucose_records");
    let value_i = column(&glucose_columns, "value");
    let glucose = merge_glucose(&glucose_columns, glucose_rows)
        .into_iter()
        .filter(|row| {
            number(row, value_i)
                .map(|value| (1.0..=35.0).contains(&value))
                .unwrap_or(false)
        })
        .collect();

    let ts_of = |columns: &[Json], row: &[Json]| {
        integer(row, column(columns, "timestamp_ms")).unwrap_or(0)
    };
    let recent = |columns: &[Json], rows: Vec<Vec<Json>>| -> Vec<Vec<Json>> {
        rows.into_iter()
            .filter(|row| ts_of(columns, row.as_slice()) >= start)
            .collect()
    };

    Ok(Json::Object(vec![
        ("code".into(), Json::Number(0.0)),
        (
            "data".into(),
            Json::Object(vec![
                (
                    "weight".into(),
                    render(&weight_columns, recent(&weight_columns, weight)),
                ),
                (
                    "blood_pressure".into(),
                    render(&pressure_columns, recent(&pressure_columns, pressure)),
                ),
                (
                    "blood_glucose".into(),
                    render(&glucose_columns, recent(&glucose_columns, glucose)),
                ),
            ]),
        ),
    ])
    .render())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(body: &str) -> Json {
        json::parse(body).unwrap()
    }

    #[test]
    fn merges_repeated_weight_packets_and_keeps_composition() {
        let snap = snapshot(
            r#"{
              "tables": {
                "weight_records": {
                  "columns": ["id","timestamp_ms","weight_kg","body_fat_percent","bmi"],
                  "rows": [
                    ["a",1793000000000,67.5,null,null],
                    ["b",1793000000500,67.5,null,23.4],
                    ["c",1793000030000,67.5,19.1,null]
                  ]
                }
              }
            }"#,
        );
        let out = project(&snap, 3650).unwrap();
        assert_eq!(out.matches(r#""weight_kg":67.5"#).count(), 1);
        assert!(out.contains(r#""body_fat_percent":19.1"#));
        assert!(out.contains(r#""bmi":23.4"#));
    }

    #[test]
    fn merges_pressure_within_one_second_only() {
        let snap = snapshot(
            r#"{
              "tables": {
                "blood_pressure_records": {
                  "columns": ["id","timestamp_ms","systolic","diastolic","pulse"],
                  "rows": [
                    ["a",1793000000000,118,76,61],
                    ["b",1793000000400,118,76,61],
                    ["c",1793000009000,118,76,61]
                  ]
                }
              }
            }"#,
        );
        let out = project(&snap, 3650).unwrap();
        assert_eq!(out.matches(r#""systolic":118"#).count(), 2);
        assert!(out.contains(r#""pulse":61"#));
    }

    #[test]
    fn merges_glucose_inside_the_same_minute() {
        let snap = snapshot(
            r#"{
              "tables": {
                "blood_glucose_records": {
                  "columns": ["id","timestamp_ms","value","measure_type"],
                  "rows": [
                    ["a",1793000000000,5.6,"FASTING"],
                    ["b",1793000005000,5.6,null],
                    ["c",1793000120000,5.9,"FASTING"]
                  ]
                }
              }
            }"#,
        );
        let out = project(&snap, 3650).unwrap();
        assert_eq!(out.matches(r#""value":5.6"#).count(), 1);
        assert_eq!(out.matches(r#""value":5.9"#).count(), 1);
        assert!(out.contains(r#""measure_type":"FASTING""#));
    }

    #[test]
    fn drops_impossible_readings_and_old_rows() {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let snap = snapshot(
            &r#"{
              "tables": {
                "weight_records": {
                  "columns": ["id","timestamp_ms","weight_kg"],
                  "rows": [["a",%NOW%,0],["b",%NOW%,67.5],["c",%OLD%,68.0]]
                },
                "blood_pressure_records": {
                  "columns": ["id","timestamp_ms","systolic","diastolic"],
                  "rows": [["a",%NOW%,10,5]]
                },
                "blood_glucose_records": {
                  "columns": ["id","timestamp_ms","value"],
                  "rows": [["a",%NOW%,99]]
                }
              }
            }"#
            .replace("%NOW%", &now.to_string())
            .replace("%OLD%", &(now - 2 * DAY_MS).to_string()),
        );
        let out = project(&snap, 1).unwrap();
        assert_eq!(out.matches(r#""weight_kg":67.5"#).count(), 1);
        assert_eq!(out.matches(r#""weight_kg":68"#).count(), 0);
        assert_eq!(out.matches(r#""systolic""#).count(), 0);
        assert_eq!(out.matches(r#""blood_glucose":[]"#).count(), 1);
    }

    #[test]
    fn missing_tables_render_empty_arrays() {
        let out = project(&snapshot(r#"{"tables":{}}"#), 30).unwrap();
        assert!(out.contains(r#""weight":[]"#));
        assert!(out.contains(r#""blood_pressure":[]"#));
        assert!(out.contains(r#""blood_glucose":[]"#));
    }
}
