//! Narrow, read-only public projection of sleep data for the blog.
//!
//! The full health snapshot stays private; this endpoint only exposes what the
//! blog's sleep card needs: one entry per night (by wake-up date), the stage
//! bands of the latest night, and the night HRV / breathing summary.
//!
//! The nightly numbers come from `sleep_sessions` (the same rows the app's
//! home page uses) so the blog and the app never disagree; `sleep_bed` only
//! refines bed / wake / score, and `sleep_stages` is used for the stage bands.

use crate::account;
#[cfg(test)]
use crate::json;
use crate::json::Json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

const DAY_MS: i64 = 86_400_000;
const CST_OFFSET_MS: i64 = 8 * 60 * 60 * 1000;
const SAME_STAGE_GAP_MS: i64 = 60 * 1000;
const MAX_BANDS: usize = 600;

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

/// `GET /blog/sleep?days=30` body.
pub fn sleep(root: &Path, days: i64) -> Result<String, String> {
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
            "sleep_sessions",
            "sleep_bed",
            "sleep_stages",
            "sleep_hrv_records",
            "metric_points",
            "daily_metrics",
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

fn cell<'a>(row: &'a Json, index: Option<usize>) -> Option<&'a Json> {
    index.and_then(|i| row.as_array().and_then(|values| values.get(i)))
}

fn number(row: &Json, index: Option<usize>) -> Option<f64> {
    cell(row, index).and_then(Json::as_f64)
}

fn integer(row: &Json, index: Option<usize>) -> Option<i64> {
    number(row, index).map(|value| value as i64)
}

/// 东八区的 epoch day（与 `daily_metrics.date_day` 同一口径）
fn epoch_day_of(ms: i64) -> i64 {
    (ms + CST_OFFSET_MS).div_euclid(DAY_MS)
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

#[derive(Clone)]
struct Session {
    start_ms: i64,
    end_ms: i64,
    deep: f64,
    light: f64,
    rem: f64,
    awake: f64,
    anchor_min: Option<f64>,
}

impl Session {
    fn asleep(&self) -> f64 {
        self.deep + self.light + self.rem
    }

    fn wall_minutes(&self) -> f64 {
        (self.end_ms - self.start_ms).max(0) as f64 / 60_000.0
    }

    /// 与 App 的 `SleepSessionRow::valid` 一致：至少睡着 5 分钟，
    /// 跨度超过 3 小时却睡不到 35% 的按假睡眠丢掉。
    fn valid(&self) -> bool {
        let asleep = self.asleep();
        asleep >= 5.0
            && !(self.wall_minutes() >= 90.0 && asleep * 100.0 < self.wall_minutes() * 35.0)
    }
}

fn sessions(snapshot: &Json) -> Vec<Session> {
    let (columns, rows) = table(snapshot, "sleep_sessions");
    let start_i = column(columns, "start_ms");
    let end_i = column(columns, "end_ms");
    let deep_i = column(columns, "deep_min");
    let light_i = column(columns, "light_min");
    let rem_i = column(columns, "rem_min");
    let awake_i = column(columns, "awake_min");
    let anchor_i = column(columns, "anchor_min");
    let mut out: Vec<Session> = rows
        .iter()
        .filter_map(|row| {
            let start_ms = integer(row, start_i)?;
            let end_ms = integer(row, end_i)?;
            if end_ms <= start_ms {
                return None;
            }
            Some(Session {
                start_ms,
                end_ms,
                deep: number(row, deep_i).unwrap_or(0.0),
                light: number(row, light_i).unwrap_or(0.0),
                rem: number(row, rem_i).unwrap_or(0.0),
                awake: number(row, awake_i).unwrap_or(0.0),
                anchor_min: number(row, anchor_i),
            })
        })
        .collect();
    out.sort_by_key(|s| s.start_ms);
    dedupe_sessions(out)
}

/// 同一晚可能被旧同步、旧库导入各写一条，区间重叠时只留睡得最长的。
fn dedupe_sessions(sessions: Vec<Session>) -> Vec<Session> {
    let mut out: Vec<Session> = Vec::with_capacity(sessions.len());
    for session in sessions {
        if let Some(prev) = out.last_mut() {
            if session.start_ms < prev.end_ms {
                if session.asleep() > prev.asleep() {
                    *prev = session;
                }
                continue;
            }
        }
        out.push(session);
    }
    out
}

/// 每晚总时长与分期：优先官方锚点时长（anchor_min），缺了就用分期累加。
fn night_totals(list: Vec<Session>) -> (i64, i64, i64, i64, i64) {
    let deep: f64 = list.iter().map(|s| s.deep).sum();
    let mut light: f64 = list.iter().map(|s| s.light).sum();
    let mut rem: f64 = list.iter().map(|s| s.rem).sum();
    let awake: f64 = list.iter().map(|s| s.awake).sum();
    let detailed = deep + light + rem;
    let anchor: f64 = list.iter().filter_map(|s| s.anchor_min).sum();
    let mut asleep = if anchor > 0.0 { anchor } else { detailed };
    if anchor > detailed {
        light += anchor - detailed;
    } else if anchor > 0.0 && detailed > anchor {
        let mut reduce = detailed - anchor;
        let from_light = light.min(reduce);
        light -= from_light;
        reduce -= from_light;
        if reduce > 0.0 {
            rem = (rem - reduce).max(0.0);
        }
    }
    if asleep <= 0.0 {
        asleep = detailed;
    }
    (
        asleep.round() as i64,
        deep.round() as i64,
        light.round() as i64,
        rem.round() as i64,
        awake.round() as i64,
    )
}

#[derive(Default, Clone)]
struct Bed {
    bed_ms: Option<i64>,
    fall_asleep_ms: Option<i64>,
    wake_ms: Option<i64>,
    score: Option<i64>,
    efficiency: Option<i64>,
}

fn beds(snapshot: &Json) -> Vec<(i64, i64, Bed)> {
    let (columns, rows) = table(snapshot, "sleep_bed");
    let session_i = column(columns, "session_start_ms");
    let bed_i = column(columns, "bed_ms");
    let fall_i = column(columns, "fall_asleep_ms");
    let wake_i = column(columns, "wake_ms");
    let score_i = column(columns, "watch_score");
    let eff_i = column(columns, "watch_efficiency");
    rows.iter()
        .filter_map(|row| {
            let session_start = integer(row, session_i)
                .or_else(|| integer(row, fall_i))
                .or_else(|| integer(row, bed_i))?;
            let bed = Bed {
                bed_ms: integer(row, bed_i),
                fall_asleep_ms: integer(row, fall_i),
                wake_ms: integer(row, wake_i),
                score: integer(row, score_i).filter(|v| (1..=100).contains(v)),
                efficiency: integer(row, eff_i).filter(|v| (1..=100).contains(v)),
            };
            Some((session_start, bed.wake_ms.unwrap_or(session_start), bed))
        })
        .collect()
}

/// 按 App 的 `watch_sleep_correction` 规则找同一场睡眠对应的官方摘要：
/// 入睡时间落在会话开始前 1 小时内 / 会话结束前，取最接近的一条。
fn bed_for<'a>(beds: &'a [(i64, i64, Bed)], start_ms: i64, end_ms: i64) -> Option<&'a Bed> {
    let mut best: Option<(i64, &Bed)> = None;
    for (session_start, wake, bed) in beds {
        let anchor = bed.fall_asleep_ms.or(bed.bed_ms).unwrap_or(*session_start);
        if anchor < start_ms - 3_600_000 || anchor > end_ms {
            continue;
        }
        let distance = (anchor - start_ms).abs();
        let better = match best {
            None => true,
            Some((best_distance, best_bed)) => {
                distance < best_distance
                    || (distance == best_distance
                        && bed.score.is_some()
                        && best_bed.score.is_none())
            }
        };
        let _ = wake;
        if better {
            best = Some((distance, bed));
        }
    }
    best.map(|(_, bed)| bed)
}

fn hrv_by_day(snapshot: &Json) -> BTreeMap<i64, (f64, Option<f64>, Option<f64>, Option<f64>)> {
    let (columns, rows) = table(snapshot, "sleep_hrv_records");
    let day_i = column(columns, "date_day");
    let avg_i = column(columns, "avg_hrv");
    let min_i = column(columns, "min_baseline");
    let max_i = column(columns, "max_baseline");
    let delta_i = column(columns, "day_to_baseline");
    let mut out = BTreeMap::new();
    for row in rows {
        let Some(day) = integer(row, day_i) else {
            continue;
        };
        let Some(avg) = number(row, avg_i).filter(|v| (1.0..=300.0).contains(v)) else {
            continue;
        };
        let valid = |value: Option<f64>| value.filter(|v| *v >= 0.0);
        out.insert(
            day,
            (
                avg,
                valid(number(row, min_i)),
                valid(number(row, max_i)),
                valid(number(row, delta_i)),
            ),
        );
    }
    out
}

fn breathing_by_day(snapshot: &Json) -> BTreeMap<i64, f64> {
    let (columns, rows) = table(snapshot, "metric_points");
    let metric_i = column(columns, "metric");
    let ts_i = column(columns, "ts_ms");
    let value_i = column(columns, "value");
    let mut sums: BTreeMap<i64, (f64, i64)> = BTreeMap::new();
    for row in rows {
        if cell(row, metric_i).and_then(Json::as_str) != Some("respiration") {
            continue;
        }
        let (Some(ts), Some(value)) = (integer(row, ts_i), number(row, value_i)) else {
            continue;
        };
        if !(1.0..=60.0).contains(&value) {
            continue;
        }
        let entry = sums.entry(epoch_day_of(ts)).or_insert((0.0, 0));
        entry.0 += value;
        entry.1 += 1;
    }
    sums.into_iter()
        .map(|(day, (sum, count))| (day, (sum / count as f64 * 10.0).round() / 10.0))
        .collect()
}

fn stage_bands(snapshot: &Json, start_ms: i64, end_ms: i64) -> Vec<(String, i64, i64)> {
    if end_ms <= start_ms {
        return Vec::new();
    }
    let (columns, rows) = table(snapshot, "sleep_stages");
    let start_i = column(columns, "start_ms");
    let end_i = column(columns, "end_ms");
    let stage_i = column(columns, "stage");
    let mut per_stage: BTreeMap<&str, Vec<(i64, i64)>> = BTreeMap::new();
    for row in rows {
        let Some(stage) = cell(row, stage_i).and_then(Json::as_str) else {
            continue;
        };
        if !matches!(stage, "deep" | "light" | "rem" | "awake") {
            continue;
        }
        let (Some(mut from), Some(mut to)) = (integer(row, start_i), integer(row, end_i)) else {
            continue;
        };
        from = from.max(start_ms);
        to = to.min(end_ms);
        if to > from {
            per_stage.entry(stage).or_default().push((from, to));
        }
    }

    let mut bands: Vec<(String, i64, i64)> = Vec::new();
    for (stage, intervals) in per_stage {
        let merged = merge_intervals(intervals);
        let mut joined: Vec<(i64, i64)> = Vec::with_capacity(merged.len());
        for (from, to) in merged {
            if let Some(last) = joined.last_mut() {
                if from - last.1 <= SAME_STAGE_GAP_MS {
                    last.1 = last.1.max(to);
                    continue;
                }
            }
            joined.push((from, to));
        }
        for (from, to) in joined {
            bands.push((stage.to_string(), from, to));
        }
    }
    bands.sort_by_key(|(_, from, to)| (*from, *to));
    bands.truncate(MAX_BANDS);
    bands
}

fn merge_intervals(mut intervals: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    if intervals.is_empty() {
        return intervals;
    }
    intervals.sort();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(intervals.len());
    for (from, to) in intervals {
        if let Some(last) = merged.last_mut() {
            if from <= last.1 {
                last.1 = last.1.max(to);
                continue;
            }
        }
        merged.push((from, to));
    }
    merged
}

/// 主睡眠阶段色块窗口：上床 → 醒来，同时必须罩住所有阶段。
fn bands_window(snapshot: &Json, start_ms: i64, end_ms: i64, bed: Option<&Bed>) -> (i64, i64) {
    let mut from = bed.and_then(|b| b.bed_ms).unwrap_or(start_ms).min(start_ms);
    let mut to = bed.and_then(|b| b.wake_ms).unwrap_or(end_ms).max(end_ms);
    let (columns, rows) = table(snapshot, "sleep_stages");
    let start_i = column(columns, "start_ms");
    let end_i = column(columns, "end_ms");
    for row in rows {
        let (Some(stage_start), Some(stage_end)) = (integer(row, start_i), integer(row, end_i))
        else {
            continue;
        };
        if stage_end <= from || stage_start >= to {
            continue;
        }
        from = from.min(stage_start);
        to = to.max(stage_end);
    }
    (from, to)
}

fn project(snapshot: &Json, days: i64) -> Result<String, String> {
    let days = days.clamp(1, 365);
    let sessions = sessions(snapshot);
    let beds = beds(snapshot);
    let hrv = hrv_by_day(snapshot);
    let breathing = breathing_by_day(snapshot);

    // 按醒来日期归组（App 口径：昨晚 23:30 睡到今早 07:00 算今天）
    let mut by_day: BTreeMap<i64, Vec<Session>> = BTreeMap::new();
    for session in sessions.into_iter().filter(Session::valid) {
        by_day
            .entry(epoch_day_of(session.end_ms))
            .or_default()
            .push(session);
    }

    let latest_day = by_day.keys().next_back().copied();
    let start_day = latest_day.unwrap_or(0).saturating_sub(days - 1);
    let mut nights: Vec<Json> = Vec::new();
    let mut detail: Option<Json> = None;

    for (day, list) in by_day.into_iter() {
        if day < start_day {
            continue;
        }
        let start_ms = list.iter().map(|s| s.start_ms).min().unwrap_or(0);
        let end_ms = list.iter().map(|s| s.end_ms).max().unwrap_or(0);
        let (total, deep, light, rem, awake) = night_totals(list.clone());
        let bed = bed_for(&beds, start_ms, end_ms);
        let bed_ms = bed.and_then(|b| b.bed_ms).unwrap_or(start_ms);
        let wake_ms = bed.and_then(|b| b.wake_ms).unwrap_or(end_ms);
        let mut efficiency = if total + awake > 0 {
            ((total as f64 / (total + awake) as f64) * 100.0).round() as i64
        } else {
            0
        };
        if let Some(official) = bed.and_then(|b| b.efficiency) {
            efficiency = official;
        }
        let score = bed.and_then(|b| b.score);
        let (hrv_avg, hrv_min, hrv_max, hrv_delta) = match hrv.get(&day) {
            Some((avg, min, max, delta)) => (Some(*avg), *min, *max, *delta),
            None => (None, None, None, None),
        };
        let breath = breathing.get(&day).copied();

        nights.push(Json::Object(vec![
            ("date".into(), Json::String(date_from_epoch_day(day))),
            ("start_time".into(), Json::Number(bed_ms as f64)),
            ("end_time".into(), Json::Number(wake_ms as f64)),
            ("duration".into(), Json::Number((total * 60) as f64)),
            ("deep".into(), Json::Number((deep * 60) as f64)),
            ("light".into(), Json::Number((light * 60) as f64)),
            ("rem".into(), Json::Number((rem * 60) as f64)),
            ("awake".into(), Json::Number((awake * 60) as f64)),
            ("nap".into(), Json::Number(0.0)),
            (
                "score".into(),
                score.map(|v| Json::Number(v as f64)).unwrap_or(Json::Null),
            ),
            ("efficiency".into(), Json::Number(efficiency as f64)),
            (
                "hrv_avg".into(),
                hrv_avg.map(Json::Number).unwrap_or(Json::Null),
            ),
            (
                "hrv_min_baseline".into(),
                hrv_min.map(Json::Number).unwrap_or(Json::Null),
            ),
            (
                "hrv_max_baseline".into(),
                hrv_max.map(Json::Number).unwrap_or(Json::Null),
            ),
            (
                "hrv_delta".into(),
                hrv_delta.map(Json::Number).unwrap_or(Json::Null),
            ),
            (
                "breathing_avg".into(),
                breath.map(Json::Number).unwrap_or(Json::Null),
            ),
        ]));

        if day == latest_day.unwrap_or(0) {
            let (from, to) = bands_window(snapshot, start_ms, end_ms, bed);
            let bands = stage_bands(snapshot, from, to);
            let longest = bands
                .iter()
                .filter(|(stage, _, _)| stage != "awake")
                .map(|(_, from, to)| (to - from) / 1000)
                .max()
                .unwrap_or(0);
            detail = Some(Json::Object(vec![
                ("date".into(), Json::String(date_from_epoch_day(day))),
                ("start_time".into(), Json::Number(from as f64)),
                ("end_time".into(), Json::Number(to as f64)),
                ("duration".into(), Json::Number((total * 60) as f64)),
                ("deep".into(), Json::Number((deep * 60) as f64)),
                ("light".into(), Json::Number((light * 60) as f64)),
                ("rem".into(), Json::Number((rem * 60) as f64)),
                ("awake".into(), Json::Number((awake * 60) as f64)),
                ("longest_band_sec".into(), Json::Number(longest as f64)),
                (
                    "stages".into(),
                    Json::Array(
                        bands
                            .into_iter()
                            .map(|(stage, from, to)| {
                                Json::Object(vec![
                                    ("stage".into(), Json::String(stage)),
                                    ("start_ms".into(), Json::Number(from as f64)),
                                    ("end_ms".into(), Json::Number(to as f64)),
                                ])
                            })
                            .collect(),
                    ),
                ),
                (
                    "hrv".into(),
                    Json::Object(vec![
                        (
                            "avg".into(),
                            hrv_avg.map(Json::Number).unwrap_or(Json::Null),
                        ),
                        (
                            "min_baseline".into(),
                            hrv_min.map(Json::Number).unwrap_or(Json::Null),
                        ),
                        (
                            "max_baseline".into(),
                            hrv_max.map(Json::Number).unwrap_or(Json::Null),
                        ),
                        (
                            "delta".into(),
                            hrv_delta.map(Json::Number).unwrap_or(Json::Null),
                        ),
                    ]),
                ),
                (
                    "breathing".into(),
                    Json::Object(vec![(
                        "avg".into(),
                        breath.map(Json::Number).unwrap_or(Json::Null),
                    )]),
                ),
            ]));
        }
    }

    nights.sort_by(|a, b| {
        let left = a.get("start_time").and_then(Json::as_f64).unwrap_or(0.0);
        let right = b.get("start_time").and_then(Json::as_f64).unwrap_or(0.0);
        right.total_cmp(&left)
    });

    Ok(Json::Object(vec![
        ("code".into(), Json::Number(0.0)),
        ("data".into(), Json::Array(nights)),
        (
            "detail".into(),
            detail.unwrap_or_else(|| Json::Object(vec![("has_data".into(), Json::Bool(false))])),
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
    fn epoch_day_uses_china_time() {
        // 2026-11-15 21:08 CST = 13:08 UTC，仍属于 11-15 这一天
        assert_eq!(epoch_day_of(1_794_748_080_000), 20_772);
        assert_eq!(date_from_epoch_day(20_772), "2026-11-15");
        assert_eq!(date_from_epoch_day(20_773), "2026-11-16");
    }

    #[test]
    fn projects_night_with_anchor_and_bed_correction() {
        let snap = snapshot(
            r#"{
              "tables": {
                "sleep_sessions": {
                  "columns": ["start_ms","end_ms","deep_min","light_min","rem_min","awake_min","anchor_min"],
                  "rows": [[1794748740000,1794782640000,118,375,78,20,543]]
                },
                "sleep_bed": {
                  "columns": ["session_start_ms","bed_ms","fall_asleep_ms","wake_ms","duration_min","watch_score","watch_efficiency"],
                  "rows": [[1794748080000,1794748080000,1794748740000,1794782640000,565,81,93]]
                },
                "sleep_stages": {
                  "columns": ["start_ms","end_ms","stage"],
                  "rows": [[1794748740000,1794750000000,"light"],[1794750000000,1794750600000,"deep"]]
                },
                "sleep_hrv_records": {
                  "columns": ["date_day","avg_hrv","min_baseline","max_baseline","day_to_baseline"],
                  "rows": [[20773,52,38,72,-4]]
                },
                "metric_points": {
                  "columns": ["metric","ts_ms","value"],
                  "rows": [["respiration",1794782940000,12]]
                },
                "daily_metrics": {"columns": ["date_day"], "rows": []}
              }
            }"#,
        );
        let out = project(&snap, 30).unwrap();
        assert!(out.contains(r#""date":"2026-11-16""#));
        // 543 分钟锚点 → 32580 秒
        assert!(out.contains(r#""duration":32580"#));
        assert!(out.contains(r#""score":81"#));
        assert!(out.contains(r#""efficiency":93"#));
        assert!(out.contains(r#""hrv_avg":52"#));
        assert!(out.contains(r#""breathing_avg":12"#));
        assert!(out.contains(r#""stage":"deep""#));
        assert!(out.contains(r#""longest_band_sec":1260"#));
    }

    #[test]
    fn skips_short_fake_sessions_and_daytime_naps() {
        let snap = snapshot(
            r#"{
              "tables": {
                "sleep_sessions": {
                  "columns": ["start_ms","end_ms","deep_min","light_min","rem_min","awake_min","anchor_min"],
                  "rows": [
                    [1794706740000,1794706800000,0,1,0,0,null],
                    [1794748740000,1794782640000,118,375,78,20,543]
                  ]
                }
              }
            }"#,
        );
        let out = project(&snap, 30).unwrap();
        assert_eq!(out.matches(r#""nap":"#).count(), 1);
        assert!(out.contains(r#""date":"2026-11-16""#));
    }

    #[test]
    fn merges_overlapping_sessions_of_the_same_night() {
        let snap = snapshot(
            r#"{
              "tables": {
                "sleep_sessions": {
                  "columns": ["start_ms","end_ms","deep_min","light_min","rem_min","awake_min","anchor_min"],
                  "rows": [
                    [1794748740000,1794782640000,118,375,78,20,543],
                    [1794748800000,1794780000000,100,300,60,10,470]
                  ]
                }
              }
            }"#,
        );
        let out = project(&snap, 30).unwrap();
        assert_eq!(out.matches(r#""nap":"#).count(), 1);
        assert!(out.contains(r#""duration":32580"#));
    }

    #[test]
    fn days_window_keeps_only_recent_nights() {
        let snap = snapshot(
            r#"{
              "tables": {
                "sleep_sessions": {
                  "columns": ["start_ms","end_ms","deep_min","light_min","rem_min","awake_min","anchor_min"],
                  "rows": [
                    [1794318000000,1794350000000,60,200,40,10,300],
                    [1794748740000,1794782640000,118,375,78,20,543]
                  ]
                }
              }
            }"#,
        );
        let out = project(&snap, 1).unwrap();
        assert_eq!(out.matches(r#""nap":"#).count(), 1);
        assert!(out.contains("2026-11-16"));
    }
}
