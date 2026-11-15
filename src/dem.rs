//! DEM（terrarium）→ 等高线 PNG 瓦片。
//!
//! 徒步地图的等高线不依赖任何第三方样式服务：上游只取 AWS Open Data 的
//! `elevation-tiles-prod/terrarium`（SRTM / GMTED，免费无 key），本机用
//! marching squares 把海拔场切成等高线，再编码成透明 PNG 交给 MapLibre
//! 当 raster 图层叠在矢量底图上。
//!
//! 这样默认底图（自托管 PMTiles）在国内也是快的：等高线瓦片由 47 自己
//! 生成，nginx 缓存 30 天；不再依赖境外的 OpenTopoMap。

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TILE: usize = 256;
const MAX_CACHE_ENTRIES: usize = 4096;
/// AWS terrarium 最高只到 z15；再往里放大由 MapLibre 自己 overzoom。
const MAX_ZOOM: u8 = 15;
const DEFAULT_UPSTREAM: &str = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium";

/// 等高距按缩放级别放宽：缩得越远，线越稀。
fn interval_for_zoom(z: u8) -> f32 {
    match z {
        0..=10 => 200.0,
        11..=12 => 100.0,
        13..=14 => 50.0,
        _ => 20.0,
    }
}

struct CacheEntry {
    data: Arc<Vec<u8>>,
    used: u64,
}

pub struct DemTiles {
    upstream: String,
    agent: ureq::Agent,
    cache: Mutex<HashMap<(u8, u32, u32), CacheEntry>>,
    tick: Mutex<u64>,
}

impl DemTiles {
    pub fn from_env() -> Self {
        let upstream = std::env::var("DEM_URL")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_UPSTREAM.to_string());
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .max_redirects(3)
            .build();
        Self {
            upstream,
            agent: config.into(),
            cache: Mutex::new(HashMap::new()),
            tick: Mutex::new(0),
        }
    }

    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    /// 返回一张 256×256 的 RGBA 等高线 PNG；海里/无数据时返回全透明图。
    pub fn contour(&self, z: u8, x: u32, y: u32) -> Result<Arc<Vec<u8>>, String> {
        let key = (z, x, y);
        if let Some(hit) = self.cache.lock().unwrap().get(&key) {
            return Ok(Arc::clone(&hit.data));
        }
        if z > MAX_ZOOM {
            return Err(format!("zoom {z} exceeds {MAX_ZOOM}"));
        }

        let png = self.fetch_dem(z, x, y)?;
        let elevation = decode_terrarium(&png)?;
        let rgba = draw_contours(&elevation, interval_for_zoom(z));
        let data = Arc::new(encode_rgba_png(TILE as u32, TILE as u32, &rgba));

        let mut tick = self.tick.lock().unwrap();
        *tick = tick.wrapping_add(1);
        let used = *tick;
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= MAX_CACHE_ENTRIES {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| *key)
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            key,
            CacheEntry {
                data: Arc::clone(&data),
                used,
            },
        );
        Ok(data)
    }

    fn fetch_dem(&self, z: u8, x: u32, y: u32) -> Result<Vec<u8>, String> {
        let url = format!("{}/{z}/{x}/{y}.png", self.upstream);
        let mut response = self
            .agent
            .get(&url)
            .header("Accept", "image/png")
            .call()
            .map_err(|error| format!("DEM 上游失败 {url}: {error}"))?;
        if response.status() != 200 {
            return Err(format!("DEM 上游 {} 返回 {}", url, response.status()));
        }
        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(4 * 1024 * 1024)
            .read_to_end(&mut body)
            .map_err(|error| format!("读取 DEM 失败：{error}"))?;
        if body.is_empty() {
            return Err("DEM 上游返回空响应".into());
        }
        Ok(body)
    }
}

/// terrarium 编码：`R * 256 + G + B / 256 - 32768`（米）。
fn terrarium_height(r: u8, g: u8, b: u8) -> f32 {
    f32::from(r) * 256.0 + f32::from(g) + f32::from(b) / 256.0 - 32768.0
}

fn decode_terrarium(png: &[u8]) -> Result<Vec<f32>, String> {
    let (width, height, rgba) = decode_png(png)?;
    let mut out = vec![0f32; width * height];
    for i in 0..width * height {
        out[i] = terrarium_height(rgba[i * 4], rgba[i * 4 + 1], rgba[i * 4 + 2]);
    }
    Ok(out)
}

/// 只认上游实际使用的格式：8 bit、非隔行、RGB 或 RGBA。
fn decode_png(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err("不是 PNG".into());
    }
    let mut pos = 8usize;
    let mut width = 0usize;
    let mut height = 0usize;
    let mut color_type = 0u8;
    let mut interlace = 0u8;
    let mut idat: Vec<u8> = Vec::new();
    while pos + 8 <= bytes.len() {
        let length = u32::from_be_bytes([
            bytes[pos],
            bytes[pos + 1],
            bytes[pos + 2],
            bytes[pos + 3],
        ]) as usize;
        let kind = &bytes[pos + 4..pos + 8];
        let start = pos + 8;
        let end = start.checked_add(length).ok_or("PNG 长度溢出")?;
        if end + 4 > bytes.len() {
            return Err("PNG 数据截断".into());
        }
        match kind {
            b"IHDR" => {
                width = u32::from_be_bytes([bytes[start], bytes[start + 1], bytes[start + 2], bytes[start + 3]]) as usize;
                height = u32::from_be_bytes([bytes[start + 4], bytes[start + 5], bytes[start + 6], bytes[start + 7]]) as usize;
                if bytes[start + 8] != 8 {
                    return Err("只支持 8 bit PNG".into());
                }
                color_type = bytes[start + 9];
                interlace = bytes[start + 12];
            }
            b"IDAT" => idat.extend_from_slice(&bytes[start..end]),
            b"IEND" => break,
            _ => {}
        }
        pos = end + 4;
    }
    if width == 0 || height == 0 || width > 2048 || height > 2048 {
        return Err("PNG 尺寸不合法".into());
    }
    if interlace != 0 {
        return Err("不支持隔行 PNG".into());
    }
    let channels = match color_type {
        2 => 3,
        6 => 4,
        _ => return Err(format!("不支持的 PNG 颜色类型 {color_type}")),
    };
    let mut raw = Vec::new();
    ZlibDecoder::new(&idat[..])
        .read_to_end(&mut raw)
        .map_err(|error| format!("解压 PNG 失败：{error}"))?;
    let stride = width * channels;
    if raw.len() < (stride + 1) * height {
        return Err("PNG 像素数据不足".into());
    }
    let mut pixels = vec![0u8; stride * height];
    for row in 0..height {
        let filter = raw[row * (stride + 1)];
        let src = &raw[row * (stride + 1) + 1..row * (stride + 1) + 1 + stride];
        let (before, current) = pixels.split_at_mut(row * stride);
        let prev: &[u8] = if row == 0 { &[] } else { &before[(row - 1) * stride..] };
        let out = &mut current[..stride];
        for i in 0..stride {
            let left = if i >= channels { out[i - channels] } else { 0 };
            let up = if prev.is_empty() { 0 } else { prev[i] };
            let up_left = if prev.is_empty() || i < channels { 0 } else { prev[i - channels] };
            out[i] = match filter {
                0 => src[i],
                1 => src[i].wrapping_add(left),
                2 => src[i].wrapping_add(up),
                3 => src[i].wrapping_add(((u16::from(left) + u16::from(up)) / 2) as u8),
                4 => src[i].wrapping_add(paeth(left, up, up_left)),
                other => return Err(format!("未知 PNG 行滤波 {other}")),
            };
        }
    }
    if channels == 4 {
        return Ok((width, height, pixels));
    }
    let mut rgba = vec![255u8; width * height * 4];
    for i in 0..width * height {
        rgba[i * 4] = pixels[i * 3];
        rgba[i * 4 + 1] = pixels[i * 3 + 1];
        rgba[i * 4 + 2] = pixels[i * 3 + 2];
    }
    Ok((width, height, rgba))
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i32::from(a) + i32::from(b) - i32::from(c);
    let pa = (p - i32::from(a)).abs();
    let pb = (p - i32::from(b)).abs();
    let pc = (p - i32::from(c)).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// marching squares 生成等高线。
///
/// 相邻瓦片在边界上取同一批角点值，所以线在瓦片接缝处天然接得上。
fn draw_contours(elevation: &[f32], interval: f32) -> Vec<u8> {
    let mut rgba = vec![0u8; TILE * TILE * 4];
    if elevation.len() < TILE * TILE || interval <= 0.0 {
        return rgba;
    }
    let at = |x: usize, y: usize| elevation[y * TILE + x];

    for y in 0..TILE - 1 {
        for x in 0..TILE - 1 {
            let corners = [at(x, y), at(x + 1, y), at(x + 1, y + 1), at(x, y + 1)];
            let min = corners.iter().cloned().fold(f32::INFINITY, f32::min);
            let max = corners.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            if !min.is_finite() || !max.is_finite() || max - min < 0.5 {
                continue;
            }
            let mut level = (min / interval).ceil() * interval;
            while level < max {
                if level > min {
                    let index = (level / (interval * 5.0)).round();
                    let is_index = (index * interval * 5.0 - level).abs() < interval * 0.01;
                    let (color, width) = if is_index {
                        ([122u8, 74, 38, 205], 2)
                    } else {
                        ([140u8, 92, 52, 115], 1)
                    };
                    emit_cell(
                        &mut rgba,
                        x,
                        y,
                        &corners,
                        level,
                        color,
                        width,
                    );
                }
                level += interval;
            }
        }
    }
    rgba
}

#[allow(clippy::too_many_arguments)]
fn emit_cell(
    rgba: &mut [u8],
    x: usize,
    y: usize,
    corners: &[f32; 4],
    level: f32,
    color: [u8; 4],
    width: i32,
) {
    // 角点顺序：左上、右上、右下、左下；边：上、右、下、左。
    let above = [
        corners[0] >= level,
        corners[1] >= level,
        corners[2] >= level,
        corners[3] >= level,
    ];
    let mut case = 0u8;
    for (i, flag) in above.iter().enumerate() {
        if *flag {
            case |= 1 << i;
        }
    }
    if case == 0 || case == 15 {
        return;
    }
    let t = |a: f32, b: f32| {
        if (b - a).abs() < f32::EPSILON {
            0.5
        } else {
            ((level - a) / (b - a)).clamp(0.0, 1.0)
        }
    };
    let fx = x as f32;
    let fy = y as f32;
    let top = (fx + t(corners[0], corners[1]), fy);
    let right = (fx + 1.0, fy + t(corners[1], corners[2]));
    let bottom = (fx + t(corners[3], corners[2]), fy + 1.0);
    let left = (fx, fy + t(corners[0], corners[3]));

    let mut segments: Vec<((f32, f32), (f32, f32))> = Vec::with_capacity(2);
    match case {
        1 => segments.push((left, top)),
        2 => segments.push((top, right)),
        3 => segments.push((left, right)),
        4 => segments.push((right, bottom)),
        5 => {
            // 鞍点：用中心均值决定两条线怎么连。
            let center = (corners[0] + corners[1] + corners[2] + corners[3]) / 4.0;
            if center >= level {
                segments.push((left, top));
                segments.push((right, bottom));
            } else {
                segments.push((left, bottom));
                segments.push((top, right));
            }
        }
        6 => segments.push((top, bottom)),
        7 => segments.push((left, bottom)),
        8 => segments.push((bottom, left)),
        9 => segments.push((top, bottom)),
        10 => {
            let center = (corners[0] + corners[1] + corners[2] + corners[3]) / 4.0;
            if center >= level {
                segments.push((top, right));
                segments.push((bottom, left));
            } else {
                segments.push((top, left));
                segments.push((bottom, right));
            }
        }
        11 => segments.push((right, bottom)),
        12 => segments.push((right, left)),
        13 => segments.push((top, right)),
        14 => segments.push((left, top)),
        _ => {}
    }
    for (from, to) in segments {
        draw_line(rgba, from, to, color, width);
    }
}

fn draw_line(
    rgba: &mut [u8],
    from: (f32, f32),
    to: (f32, f32),
    color: [u8; 4],
    width: i32,
) {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let steps = dx.abs().max(dy.abs()).ceil().max(1.0) as i32;
    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        let px = (from.0 + dx * t).round() as i32;
        let py = (from.1 + dy * t).round() as i32;
        for oy in -(width / 2)..=(width / 2) {
            for ox in -(width / 2)..=(width / 2) {
                blend_pixel(rgba, px + ox, py + oy, color);
            }
        }
    }
}

fn blend_pixel(rgba: &mut [u8], x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= TILE as i32 || y >= TILE as i32 {
        return;
    }
    let i = (y as usize * TILE + x as usize) * 4;
    let src_a = u32::from(color[3]);
    let dst_a = u32::from(rgba[i + 3]);
    let out_a = src_a + dst_a * (255 - src_a) / 255;
    if out_a == 0 {
        return;
    }
    for c in 0..3 {
        let src = u32::from(color[c]) * src_a;
        let dst = u32::from(rgba[i + c]) * dst_a * (255 - src_a) / 255;
        rgba[i + c] = ((src + dst) / out_a).min(255) as u8;
    }
    rgba[i + 3] = out_a.min(255) as u8;
}

fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0u8);
        let start = row * width as usize * 4;
        raw.extend_from_slice(&rgba[start..start + width as usize * 4]);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&raw).ok();
    let compressed = encoder.finish().unwrap_or_default();

    let mut out = Vec::with_capacity(compressed.len() + 64);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    write_chunk(&mut out, b"IHDR", &ihdr);
    write_chunk(&mut out, b"IDAT", &compressed);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

struct Crc32 {
    value: u32,
}

impl Crc32 {
    fn new() -> Self {
        Self { value: 0xffff_ffff }
    }

    fn update(&mut self, data: &[u8]) {
        for byte in data {
            let mut crc = (self.value ^ u32::from(*byte)) & 0xff;
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
            self.value = (self.value >> 8) ^ crc;
        }
    }

    fn finish(&self) -> u32 {
        self.value ^ 0xffff_ffff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrarium_round_trip() {
        // 海拔 100 m 的 terrarium 编码。
        let encoded: f32 = 100.0 + 32768.0;
        let r = (encoded / 256.0).floor() as u8;
        let g = (encoded - f32::from(r) * 256.0).floor() as u8;
        let b = ((encoded - f32::from(r) * 256.0 - f32::from(g)) * 256.0).round() as u8;
        assert!((terrarium_height(r, g, b) - 100.0).abs() < 1.0);
        assert!((terrarium_height(128, 0, 0) + 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn contour_intervals_follow_zoom() {
        assert_eq!(interval_for_zoom(8), 200.0);
        assert_eq!(interval_for_zoom(12), 100.0);
        assert_eq!(interval_for_zoom(14), 50.0);
        assert_eq!(interval_for_zoom(16), 20.0);
    }

    #[test]
    fn draws_lines_on_a_slope() {
        // 一个从 0 线性升到 300 m 的坡面，50 m 等高距应切出 5 条线。
        let mut grid = vec![0f32; TILE * TILE];
        for y in 0..TILE {
            for x in 0..TILE {
                grid[y * TILE + x] = (x as f32 / TILE as f32) * 300.0;
            }
        }
        let rgba = draw_contours(&grid, 50.0);
        let painted = rgba.chunks(4).filter(|px| px[3] > 0).count();
        assert!(painted > TILE, "坡面上应该有连续的等高线像素，实际 {painted}");
        assert!(painted < TILE * TILE / 2, "线不该铺满整张图");
    }

    #[test]
    fn flat_tile_stays_empty() {
        let grid = vec![12.0f32; TILE * TILE];
        let rgba = draw_contours(&grid, 50.0);
        assert!(rgba.chunks(4).all(|px| px[3] == 0));
    }

    #[test]
    fn png_encode_decode_round_trip() {
        let mut rgba = vec![0u8; TILE * TILE * 4];
        rgba[0] = 10;
        rgba[1] = 20;
        rgba[2] = 30;
        rgba[3] = 200;
        let png = encode_rgba_png(TILE as u32, TILE as u32, &rgba);
        let (w, h, back) = decode_png(&png).expect("decode");
        assert_eq!((w, h), (TILE, TILE));
        assert_eq!(&back[..4], &[10, 20, 30, 200]);
    }
}
