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

// WHU 的 ftp:// 上游 ureq 不支持，且其 HTTPS 端口从本机不可达。改用 BKG 的
// 公开 HTTPS 广播星历镜像（按年/年积日分目录，每天一份多系统 RINEX）。
const DEFAULT_UPSTREAM: &str = "https://igs.bkg.bund.de/root_ftp/IGS/BRDC";
/// BKG / WHU 在阿里云（47）上不可达时，退回公开 NTRIP caster 抓当前 RTCM
/// 广播星历帧（1019/1020/1042/1046）。纯 std 实现，不引入新依赖。
///
/// 用 Centipede 而不是 rtk2go：rtk2go 的 SNIP 会给所有流注入同一份静态 nav，
/// 2026-11-07 实测滞后约 140 小时，拿它当源等于给手表喂过期星历。
const DEFAULT_NTRIP: &str = "caster.centipede.fr:2101";
/// sourcetable 取不到时的兜底站点。
const NTRIP_MOUNTS: [&str; 5] = ["1CEN", "1234", "ABBEV", "35JF", "5640"];
/// 并行抓取的站点数：多站点互补可见卫星，够数就提前收工。
const NTRIP_MAX_MOUNTS: usize = 10;
const NTRIP_KEEP: [u32; 4] = [1019, 1020, 1042, 1046];
/// 理想数量（对齐 BRDC 生成时的门槛）。达不到只告警，不拒绝——少几颗
/// BDS/GAL 仍然能帮手表更快定位，总比整份星历都拿不到强。
const NTRIP_IDEAL: [(u32, usize); 4] = [(1019, 16), (1020, 8), (1042, 16), (1046, 8)];
/// 低于这个数就没有使用价值，直接判失败。
const NTRIP_MIN: [(u32, usize); 4] = [(1019, 8), (1020, 4), (1042, 6), (1046, 4)];
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

impl NavKind {}

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
    ntrip: String,
    ntrip_auth: Option<String>,
    ntrip_only: bool,
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
        let ntrip = std::env::var("EPHEMERIS_NTRIP")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_NTRIP.to_string());
        let ntrip_auth = match (
            std::env::var("EPHEMERIS_NTRIP_USER").ok(),
            std::env::var("EPHEMERIS_NTRIP_PASS").ok(),
        ) {
            (Some(user), Some(pass)) if !user.trim().is_empty() => {
                Some(base64(&format!("{}:{}", user.trim(), pass)))
            }
            _ => None,
        };
        // 国内机器上 BKG/WHU 常年不可达，设 EPHEMERIS_SOURCE=ntrip 可跳过
        // 每次几十秒的 RINEX 超时，直接走广播星历流。
        let ntrip_only = std::env::var("EPHEMERIS_SOURCE")
            .map(|value| value.trim().eq_ignore_ascii_case("ntrip"))
            .unwrap_or(false);
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .max_redirects(2)
            .build();
        Self {
            upstream,
            ntrip,
            ntrip_auth,
            ntrip_only,
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
        // BKG/WHU 这类学术网镜像在国内机器上经常整段不可达；RINEX 拿不到时
        // 退回公开 NTRIP 流，直接复用当前的广播星历帧。
        if self.ntrip_only {
            return self.make_from_ntrip(started);
        }
        let rinex_result = self.fetch_hourly();
        let rinex_error = match rinex_result {
            Ok((rinex, source)) => match self.make(&rinex, source, started) {
                Ok(value) => return Ok(value),
                Err(error) => error,
            },
            Err(error) => error,
        };
        eprintln!("RINEX 星历不可用（{rinex_error}），改用 NTRIP 广播星历");
        self.make_from_ntrip(started)
    }

    fn make_from_ntrip(&self, started: Instant) -> Result<Ephemeris, String> {
        let (host, port) = self
            .ntrip
            .split_once(':')
            .unwrap_or((self.ntrip.as_str(), "2101"));
        let port = port.parse::<u16>().unwrap_or(2101);
        // 站点表来自 caster 自己的 sourcetable：硬编码站点一旦下线/改频就会
        // 悄悄退化成「只有几颗卫星」。取不到时才用兜底列表。
        let mounts = ntrip_mounts(host, port, self.ntrip_auth.as_deref())
            .unwrap_or_else(|error| {
                eprintln!("NTRIP sourcetable 不可用（{error}），使用兜底站点");
                NTRIP_MOUNTS.iter().map(|m| m.to_string()).collect()
            });
        if mounts.is_empty() {
            return Err("NTRIP sourcetable 里没有多系统站点".into());
        }
        let mut union: BTreeMap<u32, BTreeMap<u32, (u64, Vec<u8>)>> = BTreeMap::new();
        let mut errors = Vec::new();
        // 每个流要连着听十几秒才可能凑齐一整套广播星历，串行 10 个站就是
        // 几分钟，手表那边早超时了。并行抓，谁先给够就先合。
        let mut handles = Vec::new();
        for mount in mounts.iter().take(NTRIP_MAX_MOUNTS) {
            let mount = mount.clone();
            let host = host.to_string();
            let auth = self.ntrip_auth.clone();
            handles.push(std::thread::spawn(move || {
                let result = ntrip_grab(&host, port, &mount, 18, auth.as_deref());
                (mount, result)
            }));
        }
        for handle in handles {
            let Ok((mount, result)) = handle.join() else {
                errors.push("抓取线程崩溃".to_string());
                continue;
            };
            match result {
                Ok(frames) => {
                    for (message, sv, toe, frame) in frames {
                        let table = union.entry(message).or_default();
                        let keep = table.get(&sv).map(|(old, _)| toe >= *old).unwrap_or(true);
                        if keep {
                            table.insert(sv, (toe, frame));
                        }
                    }
                }
                Err(error) => errors.push(format!("{mount}: {error}")),
            }
        }
        // 只留新鲜的 GPS 星历：过期帧会让手表算错位置，宁可少几颗。
        // 1019 的周号只有 10 bit（19.6 年回绕），当前时间必须按同一周期取模，
        // 否则现在（GPS 周 ~2400）会被算成比 toe 早 1200 多周而全部丢弃。
        let gps_cycle = 1024.0 * 604_800.0;
        let now_gps = (now_unix() - GPS_EPOCH_UNIX) % gps_cycle;
        if let Some(table) = union.get_mut(&1019) {
            table.retain(|_, (toe, _)| {
                // 差值折回 [-半个周期, 半个周期]，避免周期边界上把刚发的帧
                // 当成"晚了 19.6 年"。
                let mut age = (now_gps - (*toe as f64 % gps_cycle)) % gps_cycle;
                if age > gps_cycle / 2.0 {
                    age -= gps_cycle;
                } else if age < -gps_cycle / 2.0 {
                    age += gps_cycle;
                }
                (-FUTURE_AGE_HOURS * 3600.0..=MAX_AGE_HOURS * 3600.0).contains(&age)
            });
        }
        if let Some(missing) = NTRIP_MIN
            .iter()
            .find(|(message, need)| union.get(message).map(|t| t.len()).unwrap_or(0) < *need)
        {
            return Err(format!(
                "NTRIP 星历不足：{} 只有 {} 颗（要求 {}）；{}",
                missing.0,
                union.get(&missing.0).map(|t| t.len()).unwrap_or(0),
                missing.1,
                errors.join("; ")
            ));
        }
        for (message, ideal) in NTRIP_IDEAL {
            let count = union.get(&message).map(|t| t.len()).unwrap_or(0);
            if count < ideal {
                eprintln!("NTRIP {} 只有 {} 颗（理想 {}），继续使用", message, count, ideal);
            }
        }
        let mut rtcm = Vec::new();
        for message in NTRIP_KEEP {
            if let Some(table) = union.get(&message) {
                for (_, frame) in table.values() {
                    rtcm.extend_from_slice(frame);
                }
            }
        }
        let generated_at = now_unix().floor() as u64;
        let zip = build_zip(&rtcm, generated_at);
        let count = |message: u32| union.get(&message).map(|t| t.len()).unwrap_or(0);
        eprintln!(
            "星历生成完成（NTRIP）：{} GPS / {} GLONASS / {} BDS / {} GALILEO，{} ms",
            count(1019),
            count(1020),
            count(1042),
            count(1046),
            started.elapsed().as_millis()
        );
        Ok(Ephemeris {
            generated_at,
            source: format!("ntrip:{host}:{port}"),
            rtcm,
            zip,
            gps: count(1019),
            glo: count(1020),
            bds: count(1042),
            galileo: count(1046),
        })
    }

    fn fetch_hourly(&self) -> Result<(String, String), String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let mut requested_unix = now.as_secs_f64() - 3600.0;
        for _ in 0..3 {
            let now_parts = chrono_parts(requested_unix);
            let year = now_parts.0;
            let doy = day_of_year(now_parts.0, now_parts.1, now_parts.2);
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
                // 日播发镜像一天只有一份混合星历；取文件最小的那份即可
                // （WRD 约 1.2 MB，DLR 约 1.8 MB）。
                .take(1)
                .collect::<Vec<_>>();
            if names.is_empty() {
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
            requested_unix -= 3600.0;
        }
        Err(format!("{} 没有可用的 RINEX", self.upstream))
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
        if width > 64 || (width < 64 && value >= (1u64 << width)) {
            return Err(format!("value {value} does not fit in {width} bits"));
        }
        for index in (0..width).rev() {
            let next_bit = (value >> index) & 1;
            self.bits += 1;
            self.current = (self.current << 1) | next_bit as u8;
            if self.bits % 8 == 0 {
                self.bytes.push(self.current);
                self.current = 0;
            }
        }
        Ok(())
    }

    fn size(&self) -> usize {
        self.bits
    }

    fn payload(mut self) -> Vec<u8> {
        if self.bits % 8 != 0 {
            let padding = 8 - self.bits % 8;
            self.current <<= padding;
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
    let tail = (GLO_TAIL_268 >> (128 - 87)) as u128;
    w.put((tail >> 64) as u64, 23)?;
    w.put(tail as u64, 64)?;
    assert_eq!(w.size(), 355);
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
    (year, month, day, hour, minute, second)
}

fn leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

const MONTH_DAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 12 } else { month };
    // (153*m - 457)/5 是"当月 1 日"的 0 基年内偏移，所以要 + day - 1；
    // 旧代码写成 + day 会让每个日期整体偏大一天（GLONASS 历元因此全被当成未来）。
    365 * y + y / 4 - y / 100 + y / 400 + (153 * m - 457) / 5 + day - 1 - 719_468
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

/// 从 NTRIP caster 拉一段时间的数据流，取出 1019/1020/1042/1046 帧。
/// 返回 `(message, sv, toe, frame)`；同一个 SV 由调用方按 toe 取最新。
fn ntrip_grab(
    host: &str,
    port: u16,
    mount: &str,
    seconds: u64,
    auth: Option<&str>,
) -> Result<Vec<(u32, u32, u64, Vec<u8>)>, String> {
    use std::net::TcpStream;

    let mut stream = TcpStream::connect((host, port))
        .map_err(|error| format!("连接 {host}:{port} 失败：{error}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let auth_header = auth
        .map(|value| format!("Authorization: Basic {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET /{mount} HTTP/1.0\r\nHost: {host}\r\nNtrip-Version: Ntrip/2.0\r\n\
         User-Agent: NTRIP vitaloop-eph/1.0\r\n{auth_header}\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("发送请求失败：{error}"))?;

    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut header_end = None;
    let mut out = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if Instant::now() >= deadline {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(size) => buf.extend_from_slice(&chunk[..size]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(error) => return Err(format!("读流失败：{error}")),
        }
        if header_end.is_none() {
            header_end = find_bytes(&buf, b"\r\n\r\n").map(|index| index + 4);
            if let Some(end) = header_end {
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                if !head.lines().next().unwrap_or("").contains("200") {
                    return Err(format!(
                        "caster 拒绝：{}",
                        head.lines().next().unwrap_or("")
                    ));
                }
                buf.drain(..end);
            }
        }
        if header_end.is_some() {
            extract_rtcm(&mut buf, &mut out);
        }
    }
    if header_end.is_none() {
        return Err("没有收到 NTRIP 响应头".into());
    }
    Ok(out)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 从 caster 的 sourcetable 里挑出同时播发 1019/1020/1042/1046 的站点。
/// 返回顺序按表里的原顺序，调用方再截断并行数。
fn ntrip_mounts(host: &str, port: u16, auth: Option<&str>) -> Result<Vec<String>, String> {
    use std::net::TcpStream;

    let mut stream = TcpStream::connect((host, port))
        .map_err(|error| format!("连接 {host}:{port} 失败：{error}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let auth_header = auth
        .map(|value| format!("Authorization: Basic {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET / HTTP/1.0\r\nHost: {host}\r\nNtrip-Version: Ntrip/2.0\r\n\
         User-Agent: NTRIP vitaloop-eph/1.0\r\n{auth_header}\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("发送 sourcetable 请求失败：{error}"))?;

    let mut body = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if Instant::now() >= deadline || body.len() > 4 * 1024 * 1024 {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(size) => {
                body.extend_from_slice(&chunk[..size]);
                if find_bytes(&body, b"ENDSOURCETABLE").is_some() {
                    break;
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(error) => return Err(format!("读 sourcetable 失败：{error}")),
        }
    }
    let text = String::from_utf8_lossy(&body).to_string();
    let mut mounts = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("STR;") else {
            continue;
        };
        let fields: Vec<&str> = rest.split(';').collect();
        if fields.len() < 4 {
            continue;
        }
        let (name, format, messages) = (fields[0].trim(), fields[2], fields[3]);
        if name.is_empty() || !format.contains("RTCM") {
            continue;
        }
        // 必须同时播发 GPS/GLONASS/BDS/Galileo 的广播星历。
        if ["1019", "1020", "1042", "1046"]
            .iter()
            .all(|needle| messages.split(',').any(|item| item == *needle))
            && !mounts.iter().any(|item| item == name)
        {
            mounts.push(name.to_string());
        }
    }
    Ok(mounts)
}

/// 标准 Base64（NTRIP Basic 认证用）。项目零第三方依赖，这里只处理
/// `user:pass` 这种短 ASCII 串。
fn base64(input: &str) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) as usize & 0x3f] as char);
        out.push(TABLE[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

fn extract_rtcm(buf: &mut Vec<u8>, out: &mut Vec<(u32, u32, u64, Vec<u8>)>) {
    loop {
        let Some(start) = buf.iter().position(|byte| *byte == 0xd3) else {
            buf.clear();
            return;
        };
        if start > 0 {
            buf.drain(..start);
        }
        if buf.len() < 6 {
            return;
        }
        let length = (((buf[1] & 0x03) as usize) << 8) | buf[2] as usize;
        if buf.len() < length + 6 {
            return;
        }
        let frame = buf[..length + 6].to_vec();
        buf.drain(..length + 6);
        if crc24q(&frame[..frame.len() - 3]) != frame[frame.len() - 3..] {
            continue;
        }
        let message = ((frame[3] as u32) << 4) | ((frame[4] as u32) >> 4);
        if !NTRIP_KEEP.contains(&message) {
            continue;
        }
        let payload = &frame[3..frame.len() - 3];
        let Some(sv) = bits(payload, 12, 6) else {
            continue;
        };
        let Some(toe) = rtcm_toe(message, payload) else {
            continue;
        };
        out.push((message, sv, toe, frame));
    }
}

fn bits(payload: &[u8], offset: usize, width: usize) -> Option<u32> {
    let total = payload.len() * 8;
    if offset + width > total {
        return None;
    }
    let mut value = 0u32;
    for index in offset..offset + width {
        let byte = payload[index / 8];
        let bit = (byte >> (7 - (index % 8))) & 1;
        value = (value << 1) | bit as u32;
    }
    Some(value)
}

fn rtcm_toe(message: u32, payload: &[u8]) -> Option<u64> {
    match message {
        1019 => {
            let week = bits(payload, 18, 10)? as u64;
            let toe = bits(payload, 288, 16)? as u64 * 16;
            Some(week * 604_800 + toe)
        }
        1042 => {
            let week = bits(payload, 18, 13)? as u64;
            let toe = bits(payload, 299, 17)? as u64 * 8;
            Some((week + 1356) % 1024 * 604_800 + toe)
        }
        1046 => {
            let week = bits(payload, 18, 12)? as u64;
            let toe = bits(payload, 294, 14)? as u64 * 60;
            Some(week % 1024 * 604_800 + toe)
        }
        _ => Some(0),
    }
}

fn parse_ftp_names(listing: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in listing.lines() {
        // HTML 目录页（BKG 等 HTTPS 镜像）：<a href="BRDC...rnx.gz">
        if let Some(start) = line.find("href=\"") {
            let rest = &line[start + 6..];
            if let Some(end) = rest.find('"') {
                let name = rest[..end].trim();
                if name.ends_with(".rnx.gz") && !names.iter().any(|item| item == name) {
                    names.push(name.to_string());
                }
            }
            continue;
        }
        // 传统 FTP 列表：`-rw-r--r-- 1 ... BRDC...rnx.gz`
        let name = line.rsplit([' ', '/']).next().unwrap_or("").trim();
        if name.ends_with(".rnx.gz") && !names.iter().any(|item| item == name) {
            names.push(name.to_string());
        }
    }
    names
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
        // RINEX 3 每条记录以 6 个历元字段开头（年 月 日 时 分 秒），RINEX 2
        // 没有。上游换镜像后这里若不自适应，af0 会被读成"年"，周数整体错位。
        if looks_like_epoch(&record.vals) {
            record.vals.drain(0..6);
        }
        records.push(record);
        index = continuation;
    }
    records
}

fn looks_like_epoch(vals: &[f64]) -> bool {
    if vals.len() < 9 {
        return false;
    }
    let year = vals[0];
    let month = vals[1];
    let day = vals[2];
    let hour = vals[3];
    let minute = vals[4];
    let second = vals[5];
    year.fract() == 0.0
        && (1980.0..=2100.0).contains(&year)
        && month.fract() == 0.0
        && (1.0..=12.0).contains(&month)
        && day.fract() == 0.0
        && (1.0..=31.0).contains(&day)
        && hour.fract() == 0.0
        && (0.0..=23.0).contains(&hour)
        && minute.fract() == 0.0
        && (0.0..=59.0).contains(&minute)
        && (0.0..=60.0).contains(&second)
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
    // 先写全部本地文件头 + 数据，再集中写中央目录，最后写 EOCD。
    // 之前把中央目录条目夹在每个本地条目后面，且 EOCD 少了注释长度字段，
    // 严格解析器（unzip / Python zipfile）会直接报「找不到中央目录」。
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in &files {
        let offset = out.len() as u32;
        write_zip_local_entry(&mut out, name, data, generated_at);
        write_zip_central_entry(&mut central, name, data, generated_at, offset);
    }
    let central_directory_offset = out.len() as u32;
    let central_directory_size = central.len() as u32;
    out.extend_from_slice(&central);
    let entry_count = files.len() as u16;
    out.extend_from_slice(&0x06054b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&entry_count.to_le_bytes());
    out.extend_from_slice(&entry_count.to_le_bytes());
    out.extend_from_slice(&central_directory_size.to_le_bytes());
    out.extend_from_slice(&central_directory_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn zip_dos_stamp(generated_at: u64) -> (u16, u16) {
    let parts = chrono_parts(generated_at as f64);
    let year = parts.0.max(1980) as u32;
    let month = if parts.0 < 1980 { 1 } else { parts.1 };
    let day = if parts.0 < 1980 { 1 } else { parts.2 };
    let hour = if parts.0 < 1980 { 0 } else { parts.3 };
    let minute = if parts.0 < 1980 { 0 } else { parts.4 };
    let second = if parts.0 < 1980 { 0 } else { parts.5 };
    let dos_date = ((year - 1980) << 9) | (month << 5) | day;
    let dos_time = (hour << 11) | (minute << 5) | (second / 2);
    (dos_time as u16, dos_date as u16)
}

fn write_zip_local_entry(out: &mut Vec<u8>, name: &str, data: &[u8], generated_at: u64) {
    let crc = crc32(data);
    let (dos_time, dos_date) = zip_dos_stamp(generated_at);
    out.extend_from_slice(&0x04034b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&dos_time.to_le_bytes());
    out.extend_from_slice(&dos_date.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(data);
}

fn write_zip_central_entry(
    central: &mut Vec<u8>,
    name: &str,
    data: &[u8],
    generated_at: u64,
    offset: u32,
) {
    let crc = crc32(data);
    let (dos_time, dos_date) = zip_dos_stamp(generated_at);
    central.extend_from_slice(&0x02014b50u32.to_le_bytes());
    central.extend_from_slice(&20u16.to_le_bytes());
    central.extend_from_slice(&20u16.to_le_bytes());
    central.extend_from_slice(&0u16.to_le_bytes());
    central.extend_from_slice(&0u16.to_le_bytes());
    central.extend_from_slice(&dos_time.to_le_bytes());
    central.extend_from_slice(&dos_date.to_le_bytes());
    central.extend_from_slice(&crc.to_le_bytes());
    central.extend_from_slice(&(data.len() as u32).to_le_bytes());
    central.extend_from_slice(&(data.len() as u32).to_le_bytes());
    central.extend_from_slice(&(name.len() as u16).to_le_bytes());
    central.extend_from_slice(&0u16.to_le_bytes());
    central.extend_from_slice(&0u16.to_le_bytes());
    central.extend_from_slice(&0u16.to_le_bytes());
    central.extend_from_slice(&0u16.to_le_bytes());
    central.extend_from_slice(&0u32.to_le_bytes());
    central.extend_from_slice(&offset.to_le_bytes());
    central.extend_from_slice(name.as_bytes());
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
