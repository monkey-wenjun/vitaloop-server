//! Narrow, read-only public projection of daily fitness data for the blog.
//!
//! The full health snapshot remains private. This endpoint deliberately emits
//! only the six fields used by the blog's "daily exercise" card.

use crate::account;
#[cfg(test)]
use crate::json;
use crate::json::Json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

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

pub fn daily_fitness(root: &Path) -> Result<String, String> {
    let account = account_id(root)?;
    let dir = root.join("accounts").join(&account);
    let key = dir.display().to_string();
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

    let snapshot = account::merged_health_tables(root, &account, &["daily_metrics", "daily_rings"])
        .ok_or("health snapshot cannot be merged")?;
    let body = project(&snapshot)?;
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

fn number(row: &Json, index: Option<usize>) -> f64 {
    index
        .and_then(|i| row.as_array().and_then(|values| values.get(i)))
        .and_then(Json::as_f64)
        .unwrap_or(0.0)
}

#[derive(Default, Clone)]
struct Day {
    steps: f64,
    calories: f64,
    active_calories: f64,
    active_minutes: f64,
    distance_m: f64,
    exercise_minutes: f64,
    active_hours: f64,
}

fn project(snapshot: &Json) -> Result<String, String> {
    let mut days: BTreeMap<i64, Day> = BTreeMap::new();

    let (columns, rows) = table(snapshot, "daily_metrics");
    let day_i = column(columns, "date_day");
    let steps_i = column(columns, "steps");
    let calories_i = column(columns, "calories");
    let active_calories_i = column(columns, "active_calories");
    let active_minutes_i = column(columns, "active_minutes");
    let distance_i = column(columns, "distance_m");
    for row in rows {
        let Some(day) = day_i
            .and_then(|i| row.as_array().and_then(|values| values.get(i)))
            .and_then(Json::as_f64)
            .map(|v| v as i64)
        else {
            continue;
        };
        days.insert(
            day,
            Day {
                steps: number(row, steps_i),
                calories: number(row, calories_i),
                active_calories: number(row, active_calories_i),
                active_minutes: number(row, active_minutes_i),
                distance_m: number(row, distance_i),
                ..Day::default()
            },
        );
    }

    let (ring_columns, ring_rows) = table(snapshot, "daily_rings");
    let ring_day_i = column(ring_columns, "date_day");
    let duration_i = column(ring_columns, "duration_user_min");
    let active_hours_i = column(ring_columns, "active_user_hours");
    for row in ring_rows {
        let Some(day) = ring_day_i
            .and_then(|i| row.as_array().and_then(|values| values.get(i)))
            .and_then(Json::as_f64)
            .map(|v| v as i64)
        else {
            continue;
        };
        let entry = days.entry(day).or_default();
        entry.exercise_minutes = number(row, duration_i);
        entry.active_hours = number(row, active_hours_i);
    }

    let start = days
        .keys()
        .next_back()
        .copied()
        .unwrap_or(0)
        .saturating_sub(29);
    let records = days
        .into_iter()
        .filter(|(day, _)| *day >= start)
        .map(|(day, value)| {
            Json::Object(vec![
                ("date".into(), Json::String(date_from_epoch_day(day))),
                ("steps".into(), Json::Number(value.steps)),
                ("calories".into(), Json::Number(value.calories)),
                (
                    "active_calories".into(),
                    Json::Number(value.active_calories),
                ),
                (
                    "exercise_duration_seconds".into(),
                    Json::Number(value.exercise_minutes * 60.0),
                ),
                (
                    "active_hours".into(),
                    Json::Number(if value.active_hours > 0.0 {
                        value.active_hours
                    } else {
                        (value.active_minutes / 60.0).round()
                    }),
                ),
                ("distance_meters".into(), Json::Number(value.distance_m)),
                ("source".into(), Json::String("watch".into())),
            ])
        })
        .collect();

    Ok(Json::Object(vec![
        ("code".into(), Json::Number(0.0)),
        ("data".into(), Json::Array(records)),
    ])
    .render())
}

fn date_from_epoch_day(day: i64) -> String {
    let z = day + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day_of_month = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day_of_month:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_day_formats_as_iso_date() {
        assert_eq!(date_from_epoch_day(20_766), "2026-11-09");
        assert_eq!(date_from_epoch_day(20_767), "2026-11-10");
    }

    #[test]
    fn projects_only_blog_fitness_fields() {
        let snapshot = json::parse(
            r#"{
              "tables": {
                "daily_metrics": {
                  "columns": ["date_day","steps","calories","active_calories","distance_m","sleep_minutes"],
                  "rows": [[20767,12345,456,321,9876,420]]
                },
                "daily_rings": {
                  "columns": ["date_day","duration_user_min","active_user_hours"],
                  "rows": [[20767,52,9]]
                }
              }
            }"#,
        )
        .unwrap();
        let output = project(&snapshot).unwrap();
        assert!(output.contains(r#""date":"2026-11-10""#));
        assert!(output.contains(r#""steps":12345"#));
        assert!(output.contains(r#""exercise_duration_seconds":3120"#));
        assert!(!output.contains("sleep_minutes"));
    }
}
