//! vitaloop-server —— Vitaloop 健康数据同步、博客运动数据、地图代理与 MCP 只读接口。
//!
//! 从 awen-sync 拆出来的独立服务：
//!   · 账号 + Bearer 会话 + 健康快照（/v1/account/*、/v1/health/*、/v1/workouts*）
//!   · 博客每日运动数据（/blog/daily-fitness，只读公开）
//!   · 地图样式与瓦片代理（/style*.json、/tiles/*）
//!   · MCP Streamable HTTP（POST /mcp，JSON-RPC 2.0，Bearer 会话鉴权，全只读）
//!
//! 没有第三方依赖：HTTP、JSON、SHA-256 全部自带；flate2 只解 gzip，ureq 只向上游要瓦片。

mod account;
mod blog_health;
mod crypto;
mod json;
mod map_tiles;
mod mcp;

use map_tiles::MapTiles;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const MAX_BODY: usize = 64 * 1024 * 1024;
const BODY_TOO_LARGE: &str = "request body too large";

fn body_too_large(len: usize) -> bool {
    len > MAX_BODY
}

struct State {
    data: PathBuf,
    map: MapTiles,
}

fn main() {
    let mut bind = "127.0.0.1:8794".to_string();
    let mut data = home().join("vitaloop-server-data");

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => {
                bind = args.get(i + 1).cloned().unwrap_or(bind);
                i += 2;
            }
            "--data" => {
                data = PathBuf::from(args.get(i + 1).cloned().unwrap_or_default());
                i += 2;
            }
            other => {
                eprintln!("未知参数 {other}");
                std::process::exit(2);
            }
        }
    }

    fs::create_dir_all(&data).ok();
    eprintln!("vitaloop-server 起来：bind={bind} data={}", data.display());

    let state = Arc::new(State {
        data,
        map: MapTiles::from_env(),
    });
    eprintln!("地图瓦片上游：{}", state.map.upstream());
    let map_warm = Arc::clone(&state);
    thread::spawn(move || match map_warm.map.prewarm() {
        Ok(()) => eprintln!("地图目录已预热"),
        Err(error) => eprintln!("地图目录预热失败：{error}"),
    });
    let warm_root = state.data.clone();
    thread::spawn(move || account::warm_workout_cache(&warm_root));

    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{bind} 绑不上：{e}");
            std::process::exit(1);
        }
    };
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let st = Arc::clone(&state);
                thread::spawn(move || {
                    if let Err(e) = handle(s, &st) {
                        eprintln!("连接出错：{e}");
                    }
                });
            }
            Err(e) => eprintln!("accept 失败：{e}"),
        }
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/home/wenjun".into()))
}

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

fn handle(mut stream: TcpStream, state: &Arc<State>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let req = match read_request(&mut stream) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()),
        Err(e) => {
            if e.to_string() == BODY_TOO_LARGE {
                let _ = respond(&mut stream, 413, "text/plain", b"payload too large", None);
            } else {
                let _ = respond(&mut stream, 400, "text/plain", b"bad request", None);
            }
            return Err(e);
        }
    };

    if req.path == "/v1/health" {
        return respond(
            &mut stream,
            200,
            "application/json",
            br#"{"ok":true}"#,
            None,
        );
    }

    if req.path.starts_with("/blog/daily-fitness") {
        if req.method == "OPTIONS" {
            return respond_options(&mut stream, "GET, HEAD, OPTIONS");
        }
        if req.method != "GET" && req.method != "HEAD" {
            return respond_map(
                &mut stream,
                405,
                "text/plain",
                b"method",
                "no-cache",
                req.method == "HEAD",
            );
        }
        return match blog_health::daily_fitness(&state.data) {
            Ok(body) => respond_map(
                &mut stream,
                200,
                "application/json",
                body.as_bytes(),
                "public, max-age=60",
                req.method == "HEAD",
            ),
            Err(error) => {
                eprintln!("博客每日运动数据不可用：{error}");
                respond_map(
                    &mut stream,
                    404,
                    "text/plain",
                    b"daily fitness unavailable",
                    "no-cache",
                    req.method == "HEAD",
                )
            }
        };
    }

    if let Some(result) = route_map(&mut stream, &req, &state.map) {
        return result;
    }

    if req.path == "/mcp" || req.path.starts_with("/mcp?") {
        return mcp::handle(&mut stream, &req, &state.data);
    }

    eprintln!("REQ {} {} body={}B", req.method, req.path, req.body.len());

    if let Some((code, ctype, body)) = account::route(&req, &state.data) {
        // 健康快照按客户端上传的 gzip 字节原样保存；下载时按魔数回 Content-Encoding。
        let encoding = if req.path == "/v1/health/data" && body.starts_with(&[0x1f, 0x8b]) {
            Some("gzip")
        } else {
            None
        };
        return respond_enc(&mut stream, code, ctype, &body, encoding);
    }

    respond(&mut stream, 404, "text/plain", b"not found", None)
}

fn route_map(stream: &mut TcpStream, req: &Request, map: &MapTiles) -> Option<std::io::Result<()>> {
    let public_base = std::env::var("PUBLIC_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://sync.example.com".into());

    let style_dark = match req.path.as_str() {
        "/style.json" => Some(false),
        "/style-dark.json" => Some(true),
        _ => None,
    };
    if let Some(dark) = style_dark {
        if req.method == "OPTIONS" {
            return Some(respond_options(stream, "GET, HEAD, OPTIONS"));
        }
        if req.method != "GET" && req.method != "HEAD" {
            return Some(respond_map(
                stream,
                405,
                "text/plain",
                b"method",
                "no-cache",
                req.method == "HEAD",
            ));
        }
        let body = map.style_json(&public_base, dark);
        return Some(respond_map(
            stream,
            200,
            "application/json",
            body.as_bytes(),
            "public, max-age=300",
            req.method == "HEAD",
        ));
    }

    let parsed = parse_tile_path(&req.path)?;
    if req.method == "OPTIONS" {
        return Some(respond_options(stream, "GET, HEAD, OPTIONS"));
    }
    if req.method != "GET" && req.method != "HEAD" {
        return Some(respond_map(
            stream,
            405,
            "text/plain",
            b"method",
            "no-cache",
            req.method == "HEAD",
        ));
    }
    let Some((z, x, y)) = parsed else {
        return Some(respond_map(
            stream,
            400,
            "text/plain",
            b"bad tile coordinates",
            "no-cache",
            req.method == "HEAD",
        ));
    };
    match map.tile(z, x, y) {
        Ok(Some(body)) => Some(respond_map(
            stream,
            200,
            "application/vnd.mapbox-vector-tile",
            &body,
            "public, max-age=86400, stale-while-revalidate=604800",
            req.method == "HEAD",
        )),
        Ok(None) => Some(respond_empty_map(stream, 204, "public, max-age=3600")),
        Err(error) => {
            eprintln!("地图瓦片 {z}/{x}/{y} 失败：{error}");
            Some(respond_map(
                stream,
                502,
                "text/plain",
                b"map upstream unavailable",
                "no-cache",
                req.method == "HEAD",
            ))
        }
    }
}

fn parse_tile_path(path: &str) -> Option<Option<(u8, u32, u32)>> {
    let rest = path.strip_prefix("/tiles/")?;
    if !rest.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let mut parts = rest.split('/');
    let z = parts.next()?.parse::<u8>().ok();
    let x = parts.next()?.parse::<u32>().ok();
    let y = parts.next()?.strip_suffix(".mvt")?.parse::<u32>().ok();
    if parts.next().is_some() {
        return Some(None);
    }
    Some(z.zip(x).zip(y).map(|((z, x), y)| (z, x, y)))
}

fn respond_options(stream: &mut TcpStream, methods: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 204 No Content\r\nAllow: {methods}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: {methods}\r\nAccess-Control-Allow-Headers: Range, Content-Type\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

fn respond_map(
    stream: &mut TcpStream,
    code: u16,
    ctype: &str,
    body: &[u8],
    cache: &str,
    head_only: bool,
) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        502 => "Bad Gateway",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: {cache}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, HEAD, OPTIONS\r\nAccess-Control-Allow-Headers: Range, Content-Type\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    if !head_only {
        stream.write_all(body)?;
    }
    stream.flush()
}

fn respond_empty_map(stream: &mut TcpStream, code: u16, cache: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} No Content\r\nContent-Length: 0\r\nCache-Control: {cache}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = find_head_end(&buf) {
            break p;
        }
        if buf.len() > 64 * 1024 {
            return Ok(None);
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("").to_string();
    let path = first.next().unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if body_too_large(len) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            BODY_TOO_LARGE,
        ));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            // 客户端中途断开（超时/掉网）。绝不能把半截 body 当成完整请求，
            // 否则用户健康快照会被写成截断的 JSON。
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("请求体不完整：收到 {} / {} 字节", body.len(), len),
            ));
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn respond(
    stream: &mut TcpStream,
    code: u16,
    ctype: &str,
    body: &[u8],
    encoding: Option<&str>,
) -> std::io::Result<()> {
    respond_enc(stream, code, ctype, body, encoding)
}

fn respond_enc(
    stream: &mut TcpStream,
    code: u16,
    ctype: &str,
    body: &[u8],
    encoding: Option<&str>,
) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        code,
        reason,
        ctype,
        body.len()
    );
    if let Some(enc) = encoding {
        head.push_str(&format!("Content-Encoding: {enc}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_limit_is_inclusive_and_large_bodies_are_rejected() {
        assert!(!body_too_large(MAX_BODY));
        assert!(body_too_large(MAX_BODY + 1));
    }

    #[test]
    fn tile_paths_are_strict() {
        assert_eq!(
            parse_tile_path("/tiles/14/13660/6745.mvt"),
            Some(Some((14, 13660, 6745)))
        );
        assert_eq!(parse_tile_path("/tiles/14/-1/6745.mvt"), Some(None));
        assert_eq!(parse_tile_path("/tiles/14/13660/6745"), None);
        assert_eq!(parse_tile_path("/not-tiles/14/13660/6745.mvt"), None);
    }
}
