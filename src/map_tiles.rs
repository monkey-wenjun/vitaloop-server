//! Read-only PMTiles -> XYZ vector-tile proxy.
//!
//! The world archive is never downloaded to the server. PMTiles directories
//! and a small bounded tile cache live in memory; tile bodies are fetched from
//! the upstream archive with HTTPS Range requests.

use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HEADER_BYTES: usize = 127;
const INITIAL_RANGE_BYTES: usize = 16 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 200_000;
const MAX_DIRECTORY_CACHE: usize = 128;
const MAX_TILE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TILE_CACHE_ENTRIES: usize = 4096;
const MAX_TILE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_UPSTREAM: &str = "https://build.protomaps.com/20261109.pmtiles";

#[derive(Clone, Copy, Debug)]
struct Header {
    root_offset: u64,
    root_length: u64,
    leaf_offset: u64,
    tile_data_offset: u64,
    internal_compression: u8,
    tile_compression: u8,
    tile_type: u8,
    min_zoom: u8,
    max_zoom: u8,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    tile_id: u64,
    offset: u64,
    length: u64,
    run_length: u64,
}

#[derive(Clone)]
struct CachedTile {
    data: Arc<Vec<u8>>,
    used: u64,
}

struct TileCache {
    entries: HashMap<(u8, u32, u32), CachedTile>,
    bytes: usize,
    tick: u64,
}

impl TileCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            tick: 0,
        }
    }

    fn get(&self, key: &(u8, u32, u32)) -> Option<Arc<Vec<u8>>> {
        self.entries.get(key).map(|v| Arc::clone(&v.data))
    }

    fn prune(&mut self) {
        while self.entries.len() > MAX_TILE_CACHE_ENTRIES || self.bytes > MAX_TILE_CACHE_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, value)| value.used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(removed.data.len());
            }
        }
    }

    fn insert(&mut self, key: (u8, u32, u32), data: Arc<Vec<u8>>) {
        if data.len() > MAX_TILE_BYTES {
            return;
        }
        self.tick = self.tick.wrapping_add(1);
        if let Some(old) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old.data.len());
        }
        self.bytes += data.len();
        self.entries.insert(
            key,
            CachedTile {
                data,
                used: self.tick,
            },
        );
        self.prune();
    }
}

pub struct MapTiles {
    upstream: String,
    agent: ureq::Agent,
    header: Mutex<Option<Header>>,
    directories: Mutex<HashMap<(u64, u64), Arc<Vec<Entry>>>>,
    tiles: Mutex<TileCache>,
}

impl MapTiles {
    pub fn from_env() -> Self {
        let upstream = std::env::var("PMTILES_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_UPSTREAM.to_string());
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .max_redirects(3)
            .build();
        Self {
            upstream,
            agent: config.into(),
            header: Mutex::new(None),
            directories: Mutex::new(HashMap::new()),
            tiles: Mutex::new(TileCache::new()),
        }
    }

    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    pub fn prewarm(&self) -> Result<(), String> {
        self.header().map(|_| ())
    }

    pub fn style_json(&self, base_url: &str, dark: bool) -> String {
        let base = base_url.trim_end_matches('/');
        let (background, earth, landuse, water, road_minor, road, building, label, label_halo) =
            if dark {
                (
                    "#111827", "#151f2d", "#1a2d34", "#17395f", "#39495e", "#53657c", "#243143",
                    "#d9e2ef", "#111827",
                )
            } else {
                (
                    "#edf2f7", "#f4f1e9", "#e4eddc", "#9fc7e8", "#e3e5e2", "#ffffff", "#d8d9d6",
                    "#334155", "#ffffff",
                )
            };
        format!(
            r##"{{
  "version": 8,
  "glyphs": "https://tiles.openfreemap.org/fonts/{{fontstack}}/{{range}}.pbf",
  "sources": {{
    "protomaps": {{
      "type": "vector",
      "tiles": ["{base}/tiles/{{z}}/{{x}}/{{y}}.mvt"],
      "minzoom": 0,
      "maxzoom": 15,
      "attribution": "Protomaps · Map data © <a href=\"https://www.openstreetmap.org/copyright\">OpenStreetMap</a> contributors, ODbL"
    }}
  }},
  "layers": [
    {{"id":"background","type":"background","paint":{{"background-color":"{background}"}}}},
    {{"id":"earth","type":"fill","source":"protomaps","source-layer":"earth","paint":{{"fill-color":"{earth}"}}}},
    {{"id":"landuse","type":"fill","source":"protomaps","source-layer":"landuse","paint":{{"fill-color":"{landuse}","fill-opacity":0.72}}}},
    {{"id":"water","type":"fill","source":"protomaps","source-layer":"water","filter":["==",["geometry-type"],"Polygon"],"paint":{{"fill-color":"{water}"}}}},
    {{"id":"roads-casing","type":"line","source":"protomaps","source-layer":"roads","paint":{{"line-color":"{road_minor}","line-width":["interpolate",["linear"],["zoom"],10,0.8,14,4,18,18]}}}},
    {{"id":"roads","type":"line","source":"protomaps","source-layer":"roads","paint":{{"line-color":"{road}","line-width":["interpolate",["linear"],["zoom"],10,0.4,14,2.2,18,10]}}}},
    {{"id":"buildings","type":"fill","source":"protomaps","source-layer":"buildings","minzoom":13,"paint":{{"fill-color":"{building}","fill-opacity":["interpolate",["linear"],["zoom"],13,0,15,0.8]}}}},
    {{"id":"roads-labels-major","type":"symbol","source":"protomaps","source-layer":"roads","minzoom":11,"filter":["all",["has","name"],["in",["get","kind"],["literal",["highway","major_road"]]]],"layout":{{"symbol-placement":"line","text-field":["get","name"],"text-font":["Noto Sans Regular"],"text-size":["interpolate",["linear"],["zoom"],11,10,16,12.5],"symbol-spacing":260,"text-rotation-alignment":"map","text-pitch-alignment":"viewport"}},"paint":{{"text-color":"{label}","text-halo-color":"{label_halo}","text-halo-width":1.4}}}},
    {{"id":"roads-labels-minor","type":"symbol","source":"protomaps","source-layer":"roads","minzoom":14,"filter":["all",["has","name"],["in",["get","kind"],["literal",["minor_road","other","path"]]]],"layout":{{"symbol-placement":"line","text-field":["get","name"],"text-font":["Noto Sans Regular"],"text-size":10.5,"symbol-spacing":320,"text-rotation-alignment":"map","text-pitch-alignment":"viewport"}},"paint":{{"text-color":"{label}","text-halo-color":"{label_halo}","text-halo-width":1.2}}}},
    {{"id":"places-labels","type":"symbol","source":"protomaps","source-layer":"places","minzoom":9,"filter":["has","name"],"layout":{{"text-field":["get","name"],"text-font":["Noto Sans Regular"],"text-size":["interpolate",["linear"],["zoom"],9,11,15,14],"text-anchor":"center","text-max-width":8}},"paint":{{"text-color":"{label}","text-halo-color":"{label_halo}","text-halo-width":1.5}}}}
  ]
}}"##
        )
    }

    /// `Ok(None)` means the archive has no tile at this coordinate.
    pub fn tile(&self, z: u8, x: u32, y: u32) -> Result<Option<Arc<Vec<u8>>>, String> {
        let key = (z, x, y);
        if let Some(hit) = self.tiles.lock().unwrap().get(&key) {
            return Ok(Some(hit));
        }

        let header = self.header()?;
        if z < header.min_zoom || z > header.max_zoom {
            return Ok(None);
        }
        let Some(tile_id) = zxy_to_tile_id(z, x, y) else {
            return Ok(None);
        };

        let mut directory_offset = header.root_offset;
        let mut directory_length = header.root_length;
        for _ in 0..=3 {
            let directory = self.directory(directory_offset, directory_length)?;
            let Some(entry) = find_tile(&directory, tile_id) else {
                return Ok(None);
            };
            if entry.run_length > 0 {
                if entry.length == 0 || entry.length as usize > MAX_TILE_BYTES {
                    return Ok(None);
                }
                let raw = self.fetch_range(header.tile_data_offset + entry.offset, entry.length)?;
                let data = decompress(&raw, header.tile_compression)?;
                let data = Arc::new(data);
                self.tiles.lock().unwrap().insert(key, Arc::clone(&data));
                return Ok(Some(data));
            }
            directory_offset = header.leaf_offset + entry.offset;
            directory_length = entry.length;
        }
        Err("PMTiles directory depth exceeded".into())
    }

    fn header(&self) -> Result<Header, String> {
        if let Some(header) = *self.header.lock().unwrap() {
            return Ok(header);
        }
        let initial = self.fetch_range(0, INITIAL_RANGE_BYTES as u64)?;
        let header = parse_header(&initial)?;
        if header.root_length > 16 * 1024 * 1024 {
            return Err("PMTiles root directory length is unreasonable".into());
        }
        let root = root_slice(&initial, header)
            .map(|raw| decompress(raw, header.internal_compression))
            .transpose()?
            .map(|raw| parse_directory(&raw))
            .transpose()?
            .map(Arc::new);
        let mut guard = self.header.lock().unwrap();
        if let Some(existing) = *guard {
            return Ok(existing);
        }
        *guard = Some(header);
        drop(guard);
        if let Some(entries) = root {
            self.cache_directory((header.root_offset, header.root_length), entries);
        }
        Ok(header)
    }

    fn directory(&self, offset: u64, length: u64) -> Result<Arc<Vec<Entry>>, String> {
        let key = (offset, length);
        if let Some(hit) = self.directories.lock().unwrap().get(&key) {
            return Ok(Arc::clone(hit));
        }
        let raw = self.fetch_range(offset, length)?;
        let header = self.header()?;
        let decoded = decompress(&raw, header.internal_compression)?;
        let entries = Arc::new(parse_directory(&decoded)?);
        if entries.is_empty() {
            return Err("empty PMTiles directory".into());
        }
        self.cache_directory(key, Arc::clone(&entries));
        Ok(entries)
    }

    fn cache_directory(&self, key: (u64, u64), entries: Arc<Vec<Entry>>) {
        let mut cache = self.directories.lock().unwrap();
        if cache.len() >= MAX_DIRECTORY_CACHE {
            if let Some(oldest) = cache.keys().next().copied() {
                cache.remove(&oldest);
            }
        }
        cache.insert(key, entries);
    }

    fn fetch_range(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        if length == 0 || length > 16 * 1024 * 1024 {
            return Err(format!("invalid Range length {length}"));
        }
        let end = offset
            .checked_add(length - 1)
            .ok_or_else(|| "Range overflow".to_string())?;
        let mut response = self
            .agent
            .get(&self.upstream)
            .header("Range", &format!("bytes={offset}-{end}"))
            .header("User-Agent", "Vitaloop-map/1.0 (+https://sync.example.com)")
            .call()
            .map_err(|e| format!("upstream range request failed: {e}"))?;
        let content_length = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if content_length.is_some_and(|n| n > length) {
            return Err("upstream ignored HTTP Range".into());
        }
        let bytes = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| format!("upstream body failed: {e}"))?;
        if bytes.len() as u64 != length {
            return Err(format!(
                "short upstream range: wanted {length}, got {}",
                bytes.len()
            ));
        }
        Ok(bytes)
    }
}

fn root_slice<'a>(initial: &'a [u8], header: Header) -> Option<&'a [u8]> {
    let start = usize::try_from(header.root_offset).ok()?;
    let end = start.checked_add(usize::try_from(header.root_length).ok()?)?;
    initial.get(start..end)
}

fn parse_header(bytes: &[u8]) -> Result<Header, String> {
    if bytes.len() < HEADER_BYTES || &bytes[..7] != b"PMTiles" {
        return Err("not a PMTiles v3 archive".into());
    }
    if bytes[7] != 3 {
        return Err(format!("unsupported PMTiles version {}", bytes[7]));
    }
    let header = Header {
        root_offset: le_u64(bytes, 8)?,
        root_length: le_u64(bytes, 16)?,
        leaf_offset: le_u64(bytes, 40)?,
        tile_data_offset: le_u64(bytes, 56)?,
        internal_compression: bytes[97],
        tile_compression: bytes[98],
        tile_type: bytes[99],
        min_zoom: bytes[100],
        max_zoom: bytes[101],
    };
    if header.tile_type != 1 {
        return Err(format!(
            "unsupported PMTiles tile type {}",
            header.tile_type
        ));
    }
    if header.max_zoom > 26 || header.min_zoom > header.max_zoom {
        return Err("invalid PMTiles zoom range".into());
    }
    Ok(header)
}

fn le_u64(bytes: &[u8], at: usize) -> Result<u64, String> {
    let raw: [u8; 8] = bytes
        .get(at..at + 8)
        .ok_or_else(|| "truncated PMTiles header".to_string())?
        .try_into()
        .unwrap();
    Ok(u64::from_le_bytes(raw))
}

fn decompress(bytes: &[u8], compression: u8) -> Result<Vec<u8>, String> {
    match compression {
        1 => Ok(bytes.to_vec()),
        2 => {
            let mut out = Vec::new();
            GzDecoder::new(bytes)
                .read_to_end(&mut out)
                .map_err(|e| format!("gzip decode failed: {e}"))?;
            Ok(out)
        }
        other => Err(format!("unsupported PMTiles compression {other}")),
    }
}

fn parse_directory(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let mut pos = 0usize;
    let count = read_varint(bytes, &mut pos)?;
    let count = usize::try_from(count).map_err(|_| "directory count overflow".to_string())?;
    if count > MAX_DIRECTORY_ENTRIES {
        return Err("PMTiles directory too large".into());
    }
    let mut entries = vec![
        Entry {
            tile_id: 0,
            offset: 0,
            length: 0,
            run_length: 1,
        };
        count
    ];
    let mut last_id = 0u64;
    for entry in &mut entries {
        let delta = read_varint(bytes, &mut pos)?;
        last_id = last_id
            .checked_add(delta)
            .ok_or_else(|| "tile id overflow".to_string())?;
        entry.tile_id = last_id;
    }
    for entry in &mut entries {
        entry.run_length = read_varint(bytes, &mut pos)?;
    }
    for entry in &mut entries {
        entry.length = read_varint(bytes, &mut pos)?;
    }
    for i in 0..count {
        let value = read_varint(bytes, &mut pos)?;
        entries[i].offset = if value == 0 && i > 0 {
            entries[i - 1]
                .offset
                .checked_add(entries[i - 1].length)
                .ok_or_else(|| "tile offset overflow".to_string())?
        } else {
            value
                .checked_sub(1)
                .ok_or_else(|| "invalid first tile offset".to_string())?
        };
    }
    Ok(entries)
}

fn read_varint(bytes: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..=63).step_by(7) {
        let byte = *bytes
            .get(*pos)
            .ok_or_else(|| "truncated varint".to_string())?;
        *pos += 1;
        if shift == 63 && byte > 1 {
            return Err("varint overflow".into());
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("varint longer than 10 bytes".into())
}

fn find_tile(entries: &[Entry], tile_id: u64) -> Option<Entry> {
    let mut low = 0usize;
    let mut high = entries.len();
    while low < high {
        let mid = (low + high) / 2;
        if entries[mid].tile_id < tile_id {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    if let Some(entry) = entries.get(low).copied() {
        if entry.tile_id == tile_id {
            return Some(entry);
        }
    }
    if let Some(entry) = low
        .checked_sub(1)
        .and_then(|index| entries.get(index))
        .copied()
    {
        // run_length == 0 means this entry points at a leaf directory.
        // The preceding directory is the one that can contain the wanted tile.
        if entry.run_length == 0 {
            return Some(entry);
        }
        if entry.run_length > 0 && tile_id - entry.tile_id < entry.run_length {
            return Some(entry);
        }
    }
    None
}

fn rotate(size: i64, x: i64, y: i64, rx: i64, ry: i64) -> (i64, i64) {
    if ry == 0 {
        if rx != 0 {
            return (size - 1 - y, size - 1 - x);
        }
        return (y, x);
    }
    (x, y)
}

fn zxy_to_tile_id(z: u8, x: u32, y: u32) -> Option<u64> {
    if z > 26 {
        return None;
    }
    let n = 1i64 << z;
    let (mut tx, mut ty) = (i64::from(x), i64::from(y));
    if tx >= n || ty >= n {
        return None;
    }
    let mut acc = ((n * n - 1) / 3) as u64;
    let mut a = i32::from(z) - 1;
    let mut s = if a >= 0 { 1i64 << a } else { 0 };
    while s > 0 {
        let rx = tx & s;
        let ry = ty & s;
        acc += (((3 * rx) ^ ry) as u64) * (1u64 << a);
        (tx, ty) = rotate(s, tx, ty, rx, ry);
        s >>= 1;
        a -= 1;
    }
    Some(acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v3_header_fields() {
        let mut raw = vec![0u8; HEADER_BYTES];
        raw[..7].copy_from_slice(b"PMTiles");
        raw[7] = 3;
        raw[8..16].copy_from_slice(&127u64.to_le_bytes());
        raw[16..24].copy_from_slice(&2048u64.to_le_bytes());
        raw[40..48].copy_from_slice(&4096u64.to_le_bytes());
        raw[56..64].copy_from_slice(&8192u64.to_le_bytes());
        raw[97] = 2;
        raw[98] = 2;
        raw[99] = 1;
        raw[101] = 15;
        let header = parse_header(&raw).unwrap();
        assert_eq!(header.root_offset, 127);
        assert_eq!(header.root_length, 2048);
        assert_eq!(header.leaf_offset, 4096);
        assert_eq!(header.tile_data_offset, 8192);
        assert_eq!(header.max_zoom, 15);
    }

    #[test]
    fn parses_directory_and_finds_run() {
        // one tile id 10, run length 3, length 7, offset 100
        let bytes = [1, 10, 3, 7, 101];
        let entries = parse_directory(&bytes).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tile_id, 10);
        assert_eq!(entries[0].offset, 100);
        assert_eq!(find_tile(&entries, 12).unwrap().tile_id, 10);
        assert!(find_tile(&entries, 13).is_none());
    }

    #[test]
    fn finds_leaf_directory_before_tile_id() {
        let entries = [
            Entry {
                tile_id: 100,
                offset: 0,
                length: 10,
                run_length: 0,
            },
            Entry {
                tile_id: 200,
                offset: 10,
                length: 20,
                run_length: 0,
            },
        ];
        assert_eq!(find_tile(&entries, 150).unwrap().tile_id, 100);
        assert_eq!(find_tile(&entries, 250).unwrap().tile_id, 200);
    }

    #[test]
    fn hilbert_ids_match_official_pmtiles_values() {
        let cases = [
            (0, 0, 0, 0),
            (1, 0, 0, 1),
            (1, 0, 1, 2),
            (1, 1, 0, 4),
            (1, 1, 1, 3),
            (2, 0, 0, 5),
            (2, 0, 1, 8),
            (2, 0, 2, 9),
            (2, 0, 3, 10),
            (2, 1, 0, 6),
            (2, 1, 1, 7),
            (2, 1, 2, 12),
            (2, 1, 3, 11),
            (2, 2, 0, 19),
            (2, 2, 1, 18),
            (2, 2, 2, 13),
            (2, 2, 3, 14),
            (2, 3, 0, 20),
            (2, 3, 1, 17),
            (2, 3, 2, 16),
            (2, 3, 3, 15),
        ];
        for (z, x, y, expected) in cases {
            assert_eq!(zxy_to_tile_id(z, x, y), Some(expected), "{z}/{x}/{y}");
        }
    }

    #[test]
    fn invalid_coordinates_are_rejected() {
        assert_eq!(zxy_to_tile_id(0, 1, 0), None);
        assert_eq!(zxy_to_tile_id(27, 0, 0), None);
    }
}
