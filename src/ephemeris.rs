//! 机主本地计算的 GNSS 星历：RINEX-3 -> RTCM 3.3 -> 华为手表 AGNSS zip。
//!
//! 这里不接华为 seed/CDN，也不打包 PGNSS / ionosphere / 4056 私有帧；只由
//! 本服务的 RINEX 输入生成 `HW_AGNSS` 与 `HW_AGNSS_RTCM_33`。

use crate::json::Json;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::thread;
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
const GLO_FLAGS_265: [u8; 22] = [2, 2, 3, 4, 3, 0, 1, 2, 4, 5, 4, 1, 4, 2, 1, 4, 3, 2, 6, 3, 3, 5];

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
        self.cache
            .lock()
            .unwrap()
            .get_or_insert(current);
        self.ready.notify_all();
        Ok(())
    }

    pub fn status(&self) -> String {
        let guard = self.cache.lock().unwrap();
        let (guard, value) = match guard.as_ref() {
            Some(value) => (guard, value),
            None => return r#"{"ready":false,"source":null}"#.to_string(),
        };
        let object = Json::Object(vec![
            ("ready".into(), Json::Bool(true)),
            ("source".into(), Json::String(value.source.clone())),
            ("generated_at".into(), Json::Number(value.generated_at as f64)),
            ("gps".into(), Json::Number(value.gps as f64)),
            ("glonass".into(), Json::Number(value.glo as f64)),
            ("bds".into(), Json::Number(value.bds as f64)),
            ("galileo".into(), Json::Number(value.galileo as f64)),
            (
                "rtcm_bytes".into(),
                Json::Number(value.rtcm.len() as f64),
            ),
            ("zip_bytes".into(), Json::Number(value.zip.len() as f64)),
        ]);
        object.render()
    }

    pub fn zip(&self) -> Result<Vec<u8>, String> {
        let guard = self.cache.lock().unwrap();
        let (mut guard, value) = match guard.as_ref() {
            Some(value) => (guard, value.clone()),
            None => (self.ready.wait(guard, Duration::from_secs(20)).unwrap(), None),
        };
        while value.is_none() {
            match guard.as_ref() {
                Some(current) => return Ok(current.zip.clone()),
                None => {
                    let (next_guard, next) = self
                        .ready
                        .wait_timeout(guard, Duration::from_secs(20))
                        .unwrap();
                    guard = next_guard;
                    if next.is_none() {
                        return Err("ephemeris still refreshing".into());
                    }
                }
            }
        }
        Ok(value.unwrap().zip)
    }

    fn refresh(&self) -> Result<Ephemeris, String> {
        let started = Instant::now();
        if let Some(path) = self.local.lock().unwrap().clone() {
            let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
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
            let hour = now_parts.2;
            let base = format!("{upstream}/{year}/{doy:03}", upstream = self.upstream, year = year, doy = doy);
            let listing = self.get(format!("{base}/"))?;
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
        let mut response = self
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

        let mut glo = BTreeMap::new();
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
            rtcm.extend_from_slice(&frame(&encode_nav(entry, 1019, SC, 0.0)?));
        }
        for entry in glo.values() {
            rtcm.extend_from_slice(&frame(&encode_glo(entry)?));
        }
        for entry in bds.values() {
            rtcm.extend_from_slice(&frame(&encode_nav(entry, 1042, SC, 0.0)?));
        }
        for entry in galileo.values() {
            rtcm.extend_from_slice(&frame(&encode_nav(entry, 1046, SC, 0.0)?));
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
    bit: u32,
    bits: usize,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bit: 0,
            bits: 0,
        }
    }

    fn put(&mut self, value: u64, width: u32) -> Result<(), String> {
        if width > 64 || value >= (1u64 << width) {
            return Err(format!("value {value} does not fit in {width} bits"));
        }
        for index in (0..width).rev() {
            let next_bit = (value >> index) & 1;
            self.bytes.push((self.bytes.pop().unwrap_or(0) << 1) | next_bit as u8);
            self.bit += 1;
            self.bits += 1;
            if self.bit == 8 {
                self.bit = 0;
            } else {
                self.bytes.push(0);
            }
        }
        Ok(())
    }

    fn size(&self) -> usize {
        self.bits
    }

    fn payload(mut self) -> Vec<u8> {
        while self.bit != 0 {
            let byte = self.bytes.pop().unwrap_or(0) << 1;
            self.bytes.push(byte);
            self.bit += 1;
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
                crc = (crc << 1) ^ 0x1864cfb;
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
    let mut raw = vec![0xd3, ((payload.len() >> 8) & 0x03) as u8, payload.len() as u8];
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

fn encode_nav(entry: &NavEntry, message: u32, sc: f64, _unused: f64) -> Result<Vec<u8>, String> {
    let mut w = BitWriter::new();
    let mut put = |value: u64, width: u32| -> Result<(), String> {
        w.put(value, width)
    };
    let mut q_signed = |value: f64, scale: f64, width: u32| -> Result<(), String> {
        put(signed(quantize(value, scale), width)?, width)
    };
    let mut q_unsigned = |value: f64, scale: f64, width: u32| -> Result<(), String> {
        put(unsigned(quantize(value, scale), width)?, width)
    };
    put(message, 12)?;
    put(entry.sv as u64, 6)?;
    match entry.kind {
        NavKind::Gps => {
            put(entry.week as u64 % 1024, 10)?;
            put(entry.iode as u64, 8)?;
            put(entry.iode as u64, 10)?;
        }
        NavKind::Bds => {
            put(entry.week as u64 % 8192, 13)?;
            put(entry.iode as u64, 5)?;
            put(entry.iode as u64, 5)?;
        }
        NavKind::Galileo => {
            put(entry.week as u64 % 4096, 12)?;
            put(entry.iode as u64, 10)?;
        }
    }
    match entry.kind {
        NavKind::Gps => {
            put(0, 4)?;
            put(0, 2)?;
            q_signed(entry.idot, sc * 2f64.powi(-43), 14)?;
            q_unsigned(entry.toe, 16.0, 16)?;
            q_signed(entry.af2, 2f64.powi(-55), 8)?;
            q_signed(entry.af1, 2f64.powi(-43), 16)?;
            q_signed(entry.af0, 2f64.powi(-31), 22)?;
            q_signed(entry.crs, 2f64.powi(-5), 16)?;
            q_signed(entry.deltan, sc * 2f64.powi(-43), 16)?;
            q_signed(entry.m0, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.cuc, 2f64.powi(-29), 16)?;
            q_unsigned(entry.e, 2f64.powi(-33), 32)?;
            q_signed(entry.cus, 2f64.powi(-29), 16)?;
            q_unsigned(entry.sqrta, 2f64.powi(-19), 32)?;
            q_unsigned(entry.toe, 16.0, 16)?;
            q_signed(entry.cic, 2f64.powi(-29), 16)?;
            q_signed(entry.omg0, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.cis, 2f64.powi(-29), 16)?;
            q_signed(entry.i0, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.crc, 2f64.powi(-5), 16)?;
            q_signed(entry.omg, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.omgdot, sc * 2f64.powi(-43), 24)?;
            q_signed(entry.idot, sc * 2f64.powi(-43), 14)?;
            put(0, 2)?;
        }
        NavKind::Bds => {
            put(0, 4)?;
            q_signed(entry.idot, sc * 2f64.powi(-43), 14)?;
            q_unsigned(entry.toe, 8.0, 17)?;
            q_signed(entry.af2, 2f64.powi(-66), 11)?;
            q_signed(entry.af1, 2f64.powi(-50), 22)?;
            q_signed(entry.af0, 2f64.powi(-33), 24)?;
            q_signed(entry.crs, 2f64.powi(-6), 18)?;
            q_signed(entry.deltan, sc * 2f64.powi(-43), 16)?;
            q_signed(entry.m0, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.cuc, 2f64.powi(-31), 18)?;
            q_unsigned(entry.e, 2f64.powi(-33), 32)?;
            q_signed(entry.cus, 2f64.powi(-31), 18)?;
            q_unsigned(entry.sqrta, 2f64.powi(-19), 32)?;
            q_unsigned(entry.toe, 8.0, 17)?;
            q_signed(entry.cic, 2f64.powi(-31), 18)?;
            q_signed(entry.omg0, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.cis, 2f64.powi(-31), 18)?;
            q_signed(entry.i0, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.crc, 2f64.powi(-6), 18)?;
            q_signed(entry.omg, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.omgdot, sc * 2f64.powi(-43), 24)?;
            q_signed(entry.idot, sc * 2f64.powi(-43), 14)?;
            put(0, 8)?;
        }
        NavKind::Galileo => {
            put(0, 8)?;
            q_signed(entry.idot, sc * 2f64.powi(-43), 14)?;
            q_unsigned(entry.toe, 60.0, 14)?;
            q_signed(entry.af2, 2f64.powi(-59), 6)?;
            q_signed(entry.af1, 2f64.powi(-46), 21)?;
            q_signed(entry.af0, 2f64.powi(-34), 31)?;
            q_signed(entry.crs, 2f64.powi(-5), 16)?;
            q_signed(entry.deltan, sc * 2f64.powi(-43), 16)?;
            q_signed(entry.m0, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.cuc, 2f64.powi(-29), 16)?;
            q_unsigned(entry.e, 2f64.powi(-33), 32)?;
            q_signed(entry.cus, 2f64.powi(-29), 16)?;
            q_unsigned(entry.sqrta, 2f64.powi(-19), 32)?;
            q_unsigned(entry.toe, 60.0, 14)?;
            q_signed(entry.cic, 2f64.powi(-29), 16)?;
            q_signed(entry.omg0, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.cis, 2f64.powi(-29), 16)?;
            q_signed(entry.i0, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.crc, 2f64.powi(-5), 16)?;
            q_signed(entry.omg, sc * 2f64.powi(-31), 32)?;
            q_signed(entry.omgdot, sc * 2f64.powi(-43), 24)?;
            q_signed(entry.idot, sc * 2f64.powi(-43), 14)?;
            put(0, 10)?;
            put(0, 4)?;
        }
    }
    assert_eq!(w.size(), match entry.kind { NavKind::Gps => 488, NavKind::Bds => 512, NavKind::Galileo => 504 });
    Ok(w.payload())
}
