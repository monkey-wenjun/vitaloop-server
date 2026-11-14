//! 机主本地计算的 GNSS 星历：RINEX-3 -> RTCM 3.3 -> 华为手表 AGNSS zip。
//!
//! 这里不接华为 seed/CDN，也不打包 PGNSS / ionosphere / 4056 私有帧；只由
//! 本服务的 RINEX 输入生成 `HW_AGNSS` 与 `HW_AGNSS_RTCM_33`。

use crate::json::Json;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use ureq::Agent;

const DEFAULT_UPSTREAM: &str = "ftp://igs.gnsswhu.cn/pub/gps/data/hourly";
const GPS_EPOCH_UNIX: f64 = 315_964_800.0;
const GPS_TO_BDS_WEEK: u32 = 1356;
const GPS_TO_GST_WEEK: u32 = 1024;
const SC: f64 = std::f64::consts::PI;
const GLO_TAIL_268: u128 = 4_574_863_823_695_067_351_556_697_985_976_250_688;
const MAX_AGE_HOURS: f64 = 6.0;
const FUTURE_AGE_HOURS: f64 = 2.0;
const GLO_FLAGS_265: [u8; 22] = [
    2, 2, 3, 4, 3, 0, 1, 2, 4, 5, 4, 1, 4, 2, 1, 4, 3, 2, 6, 3, 3, 5,
];

const fn glo_flags(sv: u32) -> u8 {
    if sv >= 1 && sv <= 22 {
        GLO_FLAGS_265[(sv - 1) as usize]
    } else if sv == 24 {
        5
    } else {
        0
    }
}

#[derive(Clone, Debug)]
struct NavRecord {
    system: char,
    sv: u32,
    vals: Vec<f64>,
}

#[derive(Clone, Debug)]
struct NavEntry {
    sv: u32,
    af0: f64,
    af1: f64,
    af2: f64,
    iode: u32,
    crs: f64,
    deltan: f64,
    m0: f64,
    cuc: f64,
    e: f64,
    cus: f64,
    sqrta: f64,
    toe: f64,
    cic: f64,
    omg0: f64,
    cis: f64,
    i0: f64,
    crc: f64,
    omg: f64,
    omgdot: f64,
    idot: f64,
    week: u32,
    kind: NavKind,
}

#[derive(Clone, Copy, Debug)]
enum NavKind {
    Gps,
    Bds,
    Galileo,
}

impl NavKind {
    fn message(self) -> u32 {
        match self {
            Self::Gps => 1019,
            Self::Bds => 1042,
            Self::Galileo => 1046,
        }
    }
}

#[derive(Clone, Debug)]
struct GloEntry {
    sv: u32,
    freq: f64,
    clk: f64,
    gamma: f64,
    health: u32,
    x: f64,
    vx: f64,
    ax: f64,
    y: f64,
    vy: f64,
    ay: f64,
    z: f64,
    vz: f64,
    az: f64,
    abs_epoch: f64,
    epoch_minutes: u32,
}

#[derive(Clone, Debug)]
struct Ephemeris {
    generated_at: u64,
    source: String,
    rtcm: Vec<u8>,
    zip: Vec<u8>,
    gps: usize,
    glo: usize,
    bds: usize,
    galileo: usize,
}

pub struct EphemerisService {
    upstream: String,
    local: Mutex<Option<PathBuf>>,
    agent: Agent,
    cache: Mutex<Option<Ephemeris>>,
    ready: Condvar,
}

impl EphemerisService {
    pub fn from_env() -> Self {
        let upstream = std::env::var("EPHEMERIS_UPSTREAM")
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_UPSTREAM.to_string());
        let local = std::env::var("EPHEMERIS_RINEX")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .max_redirects(2)
            .build();
        Self {
            upstream,
            local: Mutex::new(local),
            agent: config.into(),
            cache: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    pub fn prewarm(&self) -> Result<(), String> {
        let current = self.refresh()?;
        self.cache.lock().unwrap().get_or_insert(current);
        self.ready.notify_all();
        Ok(())
    }

    pub fn status(&self) -> String {
        let guard = self.cache.lock().unwrap();
        let Some(value) = guard.as_ref() else {
            return r#"{"ready":false,"source":null}"#.to_string();
        };
        let object = Json::Object(vec![
            ("ready".into(), Json::Bool(true)),
            ("source".into(), Json::String(value.source.clone())),
            (
                "generated_at".into(),
                Json::Number(value.generated_at as f64),
            ),
            ("gps".into(), Json::Number(value.gps as f64)),
            ("glonass".into(), Json::Number(value.glo as f64)),
            ("bds".into(), Json::Number(value.bds as f64)),
            ("galileo".into(), Json::Number(value.galileo as f64)),
            ("rtcm_bytes".into(), Json::Number(value.rtcm.len() as f64)),
            ("zip_bytes".into(), Json::Number(value.zip.len() as f64)),
        ]);
        object.render()
    }

    pub fn zip(&self) -> Result<Vec<u8>, String> {
        let guard = self.cache.lock().unwrap();
        let mut guard = guard;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(current) = guard.as_ref() {
                return Ok(current.zip.clone());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("ephemeris still refreshing".into());
            }
            let (next_guard, _) = self.ready.wait_timeout(guard, remaining).unwrap();
            guard = next_guard;
        }
    }

    fn refresh(&self) -> Result<Ephemeris, String> {
        let started = Instant::now();
        if let Some(path) = self.local.lock().unwrap().clone() {
            let text = fs::read_to_string(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            return self.make(&text, format!("file:{}", path.display()), started);
        }
        let (rinex, source) = self.fetch_hourly()?;
        self.make(&rinex, source, started)
    }

    fn fetch_hourly(&self) -> Result<(String, String), String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let mut now_parts = chrono_parts(now.as_secs_f64() - 3600.0);
        for _ in 0..3 {
            let year = now_parts.0;
            let doy = now_parts.1;
            let _hour = now_parts.2;
            let base = format!(
                "{upstream}/{year}/{doy:03}",
                upstream = self.upstream,
                year = year,
                doy = doy
            );
            let listing_bytes = self.get(format!("{base}/"))?;
            let listing = String::from_utf8_lossy(&listing_bytes);
            let names = parse_ftp_names(&listing)
                .into_iter()
                .filter(|name| name.ends_with("_MN.rnx.gz"))
                .take(24)
                .collect::<Vec<_>>();
            if names.is_empty() {
                now_parts.2 = (now_parts.2 + 23) % 24;
                continue;
            }
            let mut merged = String::new();
            let mut ok = 0;
            for name in names {
                if let Ok(bytes) = self.get(format!("{base}/{name}")) {
                    if let Some(text) = gunzip(&bytes) {
                        merge_rinex_text(&mut merged, &text);
                        ok += 1;
                    }
                }
            }
            if ok > 0 && merged.contains("END OF HEADER") {
                return Ok((merged, format!("{base}/ ({ok} stations)")));
            }
            now_parts.2 = (now_parts.2 + 23) % 24;
        }
        Err("WHU hourly FTP has no usable RINEX".into())
    }

    fn get(&self, url: String) -> Result<Vec<u8>, String> {
        let response = self
            .agent
            .get(&url)
            .call()
            .map_err(|error| format!("{url}: {error}"))?;
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|error| format!("{url}: {error}"))?;
        Ok(bytes)
    }

    fn make(&self, rinex: &str, source: String, started: Instant) -> Result<Ephemeris, String> {
        let records = parse_nav(rinex);
        let glo_records = parse_glo(rinex);
        let now_unix = now_unix();
        let now_gps = now_unix - GPS_EPOCH_UNIX;
        let week = (now_gps / 604800.0).floor() as u32;

        let mut gps = BTreeMap::new();
        let mut bds = BTreeMap::new();
        let mut galileo = BTreeMap::new();
        for record in records {
            let Some(entry) = nav_entry(record, week) else {
                continue;
            };
            if !fresh_nav(&entry, now_gps) {
                continue;
            }
            let table = match entry.kind {
                NavKind::Gps => &mut gps,
                NavKind::Bds => &mut bds,
                NavKind::Galileo => &mut galileo,
            };
            match table.get(&entry.sv) {
                Some(old) if abs_toe(old) >= abs_toe(&entry) => (),
                _ => {
                    table.insert(entry.sv, entry);
                }
            }
        }

        let mut glo: BTreeMap<u32, GloEntry> = BTreeMap::new();
        for record in glo_records {
            let Some(entry) = glo_entry(record) else {
                continue;
            };
            let age_h = (now_unix - entry.abs_epoch) / 3600.0;
            if age_h < -FUTURE_AGE_HOURS || age_h > MAX_AGE_HOURS {
                continue;
            }
            match glo.get(&entry.sv) {
                Some(old) if old.abs_epoch >= entry.abs_epoch => (),
                _ => {
                    glo.insert(entry.sv, entry);
                }
            }
        }

        if gps.is_empty() || bds.is_empty() || galileo.is_empty() || glo.is_empty() {
            return Err(format!(
                "ephemeris insufficient: gps={} glonass={} bds={} galileo={}",
                gps.len(),
                glo.len(),
                bds.len(),
                galileo.len()
            ));
        }

        let mut rtcm = Vec::new();
        for entry in gps.values() {
            rtcm.extend_from_slice(&frame(&encode_nav(entry, 1019, SC)?));
        }
        for entry in glo.values() {
            rtcm.extend_from_slice(&frame(&encode_glo(entry)?));
        }
        for entry in bds.values() {
            rtcm.extend_from_slice(&frame(&encode_nav(entry, 1042, SC)?));
        }
        for entry in galileo.values() {
            rtcm.extend_from_slice(&frame(&encode_nav(entry, 1046, SC)?));
        }
        let generated_at = now_unix.floor() as u64;
        let zip = build_zip(&rtcm, generated_at);
        eprintln!(
            "星历生成完成：{} GPS / {} GLONASS / {} BDS / {} GALILEO，{} ms",
            gps.len(),
            glo.len(),
            bds.len(),
            galileo.len(),
            started.elapsed().as_millis()
        );
        Ok(Ephemeris {
            generated_at,
            source,
            rtcm,
            zip,
            gps: gps.len(),
            glo: glo.len(),
            bds: bds.len(),
            galileo: galileo.len(),
        })
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    bits: usize,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            current: 0,
            bits: 0,
        }
    }

    fn put(&mut self, value: u64, width: u32) -> Result<(), String> {
        if width > 64 || value >= (1u64 << width) {
            return Err(format!("value {value} does not fit in {width} bits"));
        }
        for index in (0..width).rev() {
            let next_bit = (value >> index) & 1;
            self.bits += 1;
            self.current = (self.current << 1) | next_bit as u8;
            if self.bits % 8 == 0 {
                self.bytes.push(self.current);
                self.current = 0;
            } else {
            }
        }
        Ok(())
    }

    fn size(&self) -> usize {
        self.bits
    }

    fn payload(mut self) -> Vec<u8> {
        while self.bits % 8 != 0 {
            self.current <<= 1;
            self.bits += 1;
        }
        if self.bits > 0 {
            self.bytes.push(self.current);
        }
        self.bytes
    }
}

struct Crc24;

impl Crc24 {
    fn update(mut crc: u32, byte: u8) -> u32 {
        crc ^= (byte as u32) << 16;
        for _ in 0..8 {
            if crc & 0x800000 != 0 {
                crc = (crc << 1) ^ 0x1864cbb;
            } else {
                crc <<= 1;
            }
        }
        crc & 0xffffff
    }
}

fn crc24q(data: &[u8]) -> [u8; 3] {
    let mut crc = 0;
    for byte in data {
        crc = Crc24::update(crc, *byte);
    }
    [(crc >> 16) as u8, (crc >> 8) as u8, crc as u8]
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut raw = vec![
        0xd3,
        ((payload.len() >> 8) & 0x03) as u8,
        payload.len() as u8,
    ];
    raw.extend_from_slice(payload);
    let crc = crc24q(&raw);
    raw.extend_from_slice(&crc);
    raw
}

fn quantize(value: f64, scale: f64) -> i64 {
    (value / scale).round() as i64
}

fn signed(value: i64, width: u32) -> Result<u64, String> {
    let bits = width;
    let min = -(1i64 << (bits - 1));
    let max = (1i64 << (bits - 1)) - 1;
    if !(min..=max).contains(&value) {
        return Err(format!("{value} does not fit signed {width} bits"));
    }
    Ok((value as u64) & ((1u64 << bits) - 1))
}

fn unsigned(value: i64, width: u32) -> Result<u64, String> {
    if value < 0 || value >= (1i64 << width) {
        return Err(format!("{value} does not fit unsigned {width} bits"));
    }
    Ok(value as u64)
}

fn encode_nav(entry: &NavEntry, message: u32, sc: f64) -> Result<Vec<u8>, String> {
    let mut w = BitWriter::new();
    macro_rules! put {
        ($value:expr, $width:expr) => {
            w.put($value as u64, $width)
        };
    }
    macro_rules! q_signed {
        ($value:expr, $scale:expr, $width:expr) => {
            put!(signed(quantize($value, $scale), $width)?, $width)
        };
    }
    macro_rules! q_unsigned {
        ($value:expr, $scale:expr, $width:expr) => {
            put!(unsigned(quantize($value, $scale), $width)?, $width)
        };
    }
    put!(message, 12)?;
    put!(entry.sv, 6)?;
    match entry.kind {
        NavKind::Gps => {
            put!(entry.week as u64 % 1024, 10)?;
            put!(entry.iode as u64, 8)?;
            put!(entry.iode as u64, 10)?;
        }
        NavKind::Bds => {
            put!(entry.week as u64 % 8192, 13)?;
            put!(entry.iode as u64, 5)?;
            put!(entry.iode as u64, 5)?;
        }
        NavKind::Galileo => {
            put!(entry.week as u64 % 4096, 12)?;
            put!(entry.iode as u64, 10)?;
        }
    }
    match entry.kind {
        NavKind::Gps => {
            put!(0, 4)?;
            put!(0, 2)?;
            q_signed!(entry.idot, sc * 2f64.powi(-43), 14)?;
            q_unsigned!(entry.toe, 16.0, 16)?;
            q_signed!(entry.af2, 2f64.powi(-55), 8)?;
            q_signed!(entry.af1, 2f64.powi(-43), 16)?;
            q_signed!(entry.af0, 2f64.powi(-31), 22)?;
            q_signed!(entry.crs, 2f64.powi(-5), 16)?;
            q_signed!(entry.deltan, sc * 2f64.powi(-43), 16)?;
            q_signed!(entry.m0, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.cuc, 2f64.powi(-29), 16)?;
            q_unsigned!(entry.e, 2f64.powi(-33), 32)?;
            q_signed!(entry.cus, 2f64.powi(-29), 16)?;
            q_unsigned!(entry.sqrta, 2f64.powi(-19), 32)?;
            q_unsigned!(entry.toe, 16.0, 16)?;
            q_signed!(entry.cic, 2f64.powi(-29), 16)?;
            q_signed!(entry.omg0, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.cis, 2f64.powi(-29), 16)?;
            q_signed!(entry.i0, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.crc, 2f64.powi(-5), 16)?;
            q_signed!(entry.omg, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.omgdot, sc * 2f64.powi(-43), 24)?;
            q_signed!(entry.idot, sc * 2f64.powi(-43), 14)?;
            put!(0, 2)?;
        }
        NavKind::Bds => {
            put!(0, 4)?;
            q_signed!(entry.idot, sc * 2f64.powi(-43), 14)?;
            q_unsigned!(entry.toe, 8.0, 17)?;
            q_signed!(entry.af2, 2f64.powi(-66), 11)?;
            q_signed!(entry.af1, 2f64.powi(-50), 22)?;
            q_signed!(entry.af0, 2f64.powi(-33), 24)?;
            q_signed!(entry.crs, 2f64.powi(-6), 18)?;
            q_signed!(entry.deltan, sc * 2f64.powi(-43), 16)?;
            q_signed!(entry.m0, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.cuc, 2f64.powi(-31), 18)?;
            q_unsigned!(entry.e, 2f64.powi(-33), 32)?;
            q_signed!(entry.cus, 2f64.powi(-31), 18)?;
            q_unsigned!(entry.sqrta, 2f64.powi(-19), 32)?;
            q_unsigned!(entry.toe, 8.0, 17)?;
            q_signed!(entry.cic, 2f64.powi(-31), 18)?;
            q_signed!(entry.omg0, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.cis, 2f64.powi(-31), 18)?;
            q_signed!(entry.i0, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.crc, 2f64.powi(-6), 18)?;
            q_signed!(entry.omg, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.omgdot, sc * 2f64.powi(-43), 24)?;
            q_signed!(entry.idot, sc * 2f64.powi(-43), 14)?;
            put!(0, 8)?;
        }
        NavKind::Galileo => {
            put!(0, 8)?;
            q_signed!(entry.idot, sc * 2f64.powi(-43), 14)?;
            q_unsigned!(entry.toe, 60.0, 14)?;
            q_signed!(entry.af2, 2f64.powi(-59), 6)?;
            q_signed!(entry.af1, 2f64.powi(-46), 21)?;
            q_signed!(entry.af0, 2f64.powi(-34), 31)?;
            q_signed!(entry.crs, 2f64.powi(-5), 16)?;
            q_signed!(entry.deltan, sc * 2f64.powi(-43), 16)?;
            q_signed!(entry.m0, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.cuc, 2f64.powi(-29), 16)?;
            q_unsigned!(entry.e, 2f64.powi(-33), 32)?;
            q_signed!(entry.cus, 2f64.powi(-29), 16)?;
            q_unsigned!(entry.sqrta, 2f64.powi(-19), 32)?;
            q_unsigned!(entry.toe, 60.0, 14)?;
            q_signed!(entry.cic, 2f64.powi(-29), 16)?;
            q_signed!(entry.omg0, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.cis, 2f64.powi(-29), 16)?;
            q_signed!(entry.i0, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.crc, 2f64.powi(-5), 16)?;
            q_signed!(entry.omg, sc * 2f64.powi(-31), 32)?;
            q_signed!(entry.omgdot, sc * 2f64.powi(-43), 24)?;
            q_signed!(entry.idot, sc * 2f64.powi(-43), 14)?;
            put!(0, 10)?;
            put!(0, 4)?;
        }
    }
    assert_eq!(
        w.size(),
        match entry.kind {
            NavKind::Gps => 488,
            NavKind::Bds => 512,
            NavKind::Galileo => 504,
        }
    );
    Ok(w.payload())
}

fn encode_glo(entry: &GloEntry) -> Result<Vec<u8>, String> {
    let mut w = BitWriter::new();
    w.put(1020, 12)?;
    w.put(entry.sv as u64, 6)?;
    w.put(unsigned(entry.freq.round() as i64 + 7, 5)?, 5)?;
    w.put(entry.health as u64 & 1, 1)?;
    w.put(0, 1)?;
    w.put(1, 2)?;
    w.put(0, 12)?;
    w.put(0, 1)?;
    w.put(1, 1)?;
    let tb = ((entry.epoch_minutes + 180) % 1440) / 15;
    w.put(tb as u64, 7)?;
    for (velocity, position, acceleration) in [
        (&entry.vx, &entry.x, &entry.ax),
        (&entry.vy, &entry.y, &entry.ay),
        (&entry.vz, &entry.z, &entry.az),
    ] {
        w.put(sign_magnitude(*velocity, 2f64.powi(-20), 24)?, 24)?;
        w.put(sign_magnitude(*position, 2f64.powi(-11), 27)?, 27)?;
        w.put(sign_magnitude(*acceleration, 2f64.powi(-30), 5)?, 5)?;
    }
    w.put(1, 1)?;
    w.put(sign_magnitude(entry.gamma, 2f64.powi(-40), 11)?, 11)?;
    w.put(0, 3)?;
    let tau = quantize(entry.clk, 2f64.powi(-36));
    w.put(if tau >= 0 { 1 } else { 0 }, 1)?;
    w.put(unsigned(tau.abs(), 27)?, 27)?;
    w.put(0, 6)?;
    w.put(glo_flags(entry.sv) as u64, 3)?;
    w.put(GLO_TAIL_268 as u64, 64)?;
    w.put((GLO_TAIL_268 >> 64) as u64, 23)?;
    assert_eq!(w.size(), 360);
    Ok(w.payload())
}

fn sign_magnitude(value: f64, scale: f64, width: u32) -> Result<u64, String> {
    let quantized = quantize(value, scale);
    let magnitude = quantized.abs();
    if magnitude >= 1i64 << (width - 1) {
        return Err(format!("GLONASS field overflow: {value}"));
    }
    Ok(if quantized < 0 {
        (1u64 << (width - 1)) | magnitude as u64
    } else {
        magnitude as u64
    })
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn chrono_parts(unix: f64) -> (i64, u32, u32, u32, u32, u32) {
    let seconds = unix.floor() as i64;
    let days = seconds.div_euclid(86400);
    let mut remaining = seconds.rem_euclid(86400);
    let hour = (remaining / 3600) as u32;
    remaining %= 3600;
    let minute = (remaining / 60) as u32;
    let second = (remaining % 60) as u32;

    let mut year = 1970 + days.div_euclid(365);
    while days >= civil_from_days(year + 1, 1, 1) {
        year += 1;
    }
    let mut month = 1;
    while month < 12 && days >= civil_from_days(year, month as i64 + 1, 1) {
        month += 1;
    }
    let day = (days - civil_from_days(year, month as i64, 1) + 1) as u32;
    (year, day_of_year(year, month, day), hour, minute, second, 0)
}

fn leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

const MONTH_DAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 12 } else { month };
    365 * y + y / 4 - y / 100 + y / 400 + (153 * m - 457) / 5 + day - 719_468
}

fn civil_from_days(year: i64, month: i64, day: i64) -> i64 {
    days_from_civil(year, month, day)
}

fn day_of_year(year: i64, month: u32, day: u32) -> u32 {
    let mut result = day;
    for month_index in 1..month {
        result += MONTH_DAYS[(month_index - 1) as usize];
        if month_index == 2 && leap_year(year) {
            result += 1;
        }
    }
    result
}

fn parse_ftp_names(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let name = line.rsplit([' ', '/']).next()?.trim();
            (!name.is_empty() && name.ends_with(".rnx.gz")).then(|| name.to_string())
        })
        .collect()
}

fn gunzip(bytes: &[u8]) -> Option<String> {
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut text = String::new();
    decoder.read_to_string(&mut text).ok()?;
    Some(text)
}

fn merge_rinex_text(out: &mut String, text: &str) {
    let mut header_lines = text.lines();
    let mut version = String::new();
    for line in header_lines.by_ref() {
        if line.contains("RINEX VERSION / TYPE") {
            version = line.to_string();
        }
        if line.contains("END OF HEADER") {
            break;
        }
    }
    if !out.contains("END OF HEADER") {
        out.push_str(version.trim_end());
        out.push('\n');
        out.push_str("SP                       MERGED HOURLY NAV             \n");
        for _ in 0..80 {
            out.push_str("                                                        \n");
        }
        out.push_str("                                                        END OF HEADER\n");
    }
    for line in header_lines {
        out.push_str(line.trim_end());
        out.push('\n');
    }
}

fn parse_nav(text: &str) -> Vec<NavRecord> {
    let mut records = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.len() < 23 {
            index += 1;
            continue;
        }
        let first = line.as_bytes()[0] as char;
        if !matches!(first, 'G' | 'C' | 'E') || line[1..3].trim().parse::<u32>().is_err() {
            index += 1;
            continue;
        }
        let vals = parse_floats(&line[3..]);
        if vals.len() < 8 {
            index += 1;
            continue;
        }
        let mut record = NavRecord {
            system: first,
            sv: line[1..3].trim().parse().unwrap(),
            vals,
        };
        let mut continuation = index + 1;
        while continuation < lines.len() && lines[continuation].starts_with("   ") {
            record.vals.extend(parse_floats(lines[continuation]));
            continuation += 1;
            if record.vals.len() > 32 {
                break;
            }
        }
        records.push(record);
        index = continuation;
    }
    records
}

fn parse_glo(text: &str) -> Vec<GloEntry> {
    let mut records = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.len() < 23 || !line.starts_with('R') || line[1..3].trim().parse::<u32>().is_err() {
            index += 1;
            continue;
        }
        let tokens = parse_floats(&line[3..]);
        if tokens.len() < 9 {
            index += 1;
            continue;
        }
        let sv = line[1..3].trim().parse().unwrap();
        let epoch = &tokens[0..6];
        let mut body = Vec::new();
        let mut continuation = index + 1;
        while continuation < lines.len()
            && lines[continuation].starts_with("   ")
            && body.len() < 12
        {
            body.extend(parse_floats(lines[continuation]));
            continuation += 1;
        }
        index = continuation;
        if body.len() < 12 {
            continue;
        }
        let year = epoch.first().copied().map(|value| value as i64);
        let month = epoch.get(1).copied().map(|value| value as i64);
        let day = epoch.get(2).copied().map(|value| value as i64);
        let hour = epoch.get(3).copied().map(|value| value as i64);
        let minute = epoch.get(4).copied().map(|value| value as i64);
        let Some((year, month, day, hour, minute)) = year
            .zip(month)
            .zip(day)
            .zip(hour)
            .zip(minute)
            .map(|((((year, month), day), hour), minute)| (year, month, day, hour, minute))
        else {
            continue;
        };
        let seconds = days_from_civil(year, month, day) * 86400
            + hour * 3600
            + minute * 60
            + epoch[5].round() as i64;
        let entry = GloEntry {
            sv,
            freq: body[7],
            clk: tokens[6],
            gamma: tokens[7],
            health: body[3].round() as u32,
            x: body[0],
            vx: body[1],
            ax: body[2],
            y: body[4],
            vy: body[5],
            ay: body[6],
            z: body[8],
            vz: body[9],
            az: body[10],
            abs_epoch: seconds as f64,
            epoch_minutes: (hour * 60 + minute) as u32,
        };
        records.push(entry);
    }
    records
}

fn glo_entry(record: GloEntry) -> Option<GloEntry> {
    let coordinates_ok = record.x.abs().max(record.y.abs()).max(record.z.abs()) < 60_000.0;
    let velocity_ok = record.vx.abs().max(record.vy.abs()).max(record.vz.abs()) < 12.0;
    if record.freq.abs() <= 7.0
        && record.clk.abs() < 1e-2
        && record.gamma.abs() < 1e-9
        && coordinates_ok
        && velocity_ok
    {
        Some(record)
    } else {
        None
    }
}

fn parse_floats(line: &str) -> Vec<f64> {
    let mut output = Vec::new();
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        if bytes[index] == b'-' || bytes[index] == b'+' {
            index += 1;
        }
        let mut digits = false;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
            digits = true;
        }
        if index < bytes.len() && bytes[index] == b'.' {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
                digits = true;
            }
        }
        if !digits {
            index = start + 1;
            continue;
        }
        if index < bytes.len()
            && (bytes[index] == b'e'
                || bytes[index] == b'E'
                || bytes[index] == b'd'
                || bytes[index] == b'D')
        {
            let exponent_at = index;
            index += 1;
            if index < bytes.len() && (bytes[index] == b'-' || bytes[index] == b'+') {
                index += 1;
            }
            if index < bytes.len() && bytes[index].is_ascii_digit() {
                while index < bytes.len() && bytes[index].is_ascii_digit() {
                    index += 1;
                }
            } else {
                index = exponent_at;
            }
        }
        if let Ok(value) = line[start..index].replace(['D', 'd'], "E").parse::<f64>() {
            output.push(value);
        }
    }
    output
}

fn nav_entry(record: NavRecord, _current_week: u32) -> Option<NavEntry> {
    let v = record.vals;
    let (kind, week) = match record.system {
        'G' if v.len() >= 27 => (NavKind::Gps, v[21].round() as u32),
        'C' if v.len() >= 25 => (NavKind::Bds, v[20].round() as u32 + GPS_TO_BDS_WEEK),
        'E' if v.len() >= 20 => {
            let mut week = v[21].round() as u32;
            if week < 2000 {
                week += GPS_TO_GST_WEEK;
            }
            (NavKind::Galileo, week)
        }
        _ => return None,
    };
    Some(NavEntry {
        sv: record.sv,
        af0: v[0],
        af1: v[1],
        af2: v[2],
        iode: v[3].round() as u32,
        crs: v[4],
        deltan: v[5],
        m0: v[6],
        cuc: v[7],
        e: v[8],
        cus: v[9],
        sqrta: v[10],
        toe: v[11],
        cic: v[12],
        omg0: v[13],
        cis: v[14],
        i0: v[15],
        crc: v[16],
        omg: v[17],
        omgdot: v[18],
        idot: v[19],
        week,
        kind,
    })
}

fn abs_toe(entry: &NavEntry) -> f64 {
    entry.week as f64 * 604800.0 + entry.toe
}

fn fresh_nav(entry: &NavEntry, now_gps: f64) -> bool {
    let age_hours = (now_gps - abs_toe(entry)) / 3600.0;
    age_hours >= -FUTURE_AGE_HOURS && age_hours <= MAX_AGE_HOURS
}

fn build_zip(rtcm: &[u8], generated_at: u64) -> Vec<u8> {
    let gz_header = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(rtcm).unwrap();
    let compressed = gz.finish().unwrap();
    let mut gz_data = gz_header;
    gz_data.extend_from_slice(&compressed[10..compressed.len() - 8]);
    gz_data.extend_from_slice(&crc32(rtcm).to_le_bytes());
    gz_data.extend_from_slice(&(rtcm.len() as u32).to_le_bytes());

    let config = format!(
        "{{\n  \"higeo/v1/gnssinfo?type=0x0024/HW_AGNSS\": {{\n    \"files\": [\"HW_AGNSS\", \"HW_AGNSS_RTCM_33\"],\n    \"tag\": \"higeo/v1/gnssinfo?type=0x0024/HW_AGNSS\",\n    \"uuid\": \"agnss-{}\",\n    \"ver\": 3\n  }}\n}}\n",
        chrono_parts(now_unix()).0
    );
    let files = [
        ("time", generated_at.to_string().into_bytes()),
        ("ephemeris_config.json", config.into_bytes()),
        ("agnss/HW_AGNSS_RTCM_33", rtcm.to_vec()),
        ("agnss/HW_AGNSS", gz_data),
    ];
    let mut out = Vec::new();
    let entry_count = files.len();
    for (name, data) in files {
        write_zip_entry(&mut out, name, &data, generated_at);
    }
    let size = out.len() as u32;
    out.extend_from_slice(&0x06054b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entry_count as u16).to_le_bytes());
    out.extend_from_slice(&(entry_count as u16).to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

fn write_zip_entry(out: &mut Vec<u8>, name: &str, data: &[u8], generated_at: u64) {
    let crc = crc32(data);
    let offset = out.len() as u32;
    let parts = chrono_parts(generated_at as f64);
    let year = parts.0.max(1980) as u32;
    let month = if parts.0 < 1980 { 1 } else { parts.1 };
    let day = if parts.0 < 1980 { 1 } else { parts.2 };
    let hour = if parts.0 < 1980 { 0 } else { parts.3 };
    let minute = if parts.0 < 1980 { 0 } else { parts.3 };
    let second = if parts.0 < 1980 { 0 } else { parts.4 };
    let dos_date = ((year - 1980) << 9) | (month << 5) | day;
    let dos_time = (hour << 11) | (minute << 5) | (second / 2);
    out.extend_from_slice(&0x04034b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(dos_time as u16).to_le_bytes());
    out.extend_from_slice(&(dos_date as u16).to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&0x02014b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(dos_time as u16).to_le_bytes());
    out.extend_from_slice(&(dos_date as u16).to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffffffffu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[cfg(test)]
#[path = "ephemeris_tests.rs"]
mod ephemeris_tests;
