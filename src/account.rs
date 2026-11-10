//! 邮箱账号 + Bearer 会话 + 健康快照。
//!
//! 阅读器原来的设备签名接口不动；这里只给 Vitaloop 提供可选的账号云同步：
//!   POST /v1/account/register {"email","password"}
//!   POST /v1/account/login    {"email","password"}
//!   GET  /v1/account/me       Authorization: Bearer <token>
//!   GET/PUT /v1/health/data   Authorization: Bearer <token>
//!   PUT  /v1/health/workouts  Authorization: Bearer <token>
//!   GET  /v1/workouts         Authorization: Bearer <token>
//!   GET  /v1/workouts/{id}    Authorization: Bearer <token>
//!   GET  /v1/app/config       公开，返回 App 版本与下载地址
//!
//! 密码只存加盐迭代 SHA-256；会话 30 天，落盘到 accounts/sessions。

use crate::crypto::*;
use crate::json::{self, Json};
use crate::Request;
use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const PASSWORD_ROUNDS: usize = 60_000;
const SESSION_SECONDS: i64 = 30 * 24 * 3600;

static SESSION_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

struct WorkoutCacheEntry {
    health: FileStamp,
    workouts: FileStamp,
    rows: Arc<Vec<Json>>,
}

static WORKOUT_CACHE: OnceLock<Mutex<HashMap<PathBuf, WorkoutCacheEntry>>> = OnceLock::new();
static WORKOUT_LOAD_LOCK: Mutex<()> = Mutex::new(());

/// 启动时预解析所有账号的运动表；首个公开请求不再承担几十兆 JSON 的冷解析。
pub fn warm_workout_cache(root: &Path) {
    let Ok(entries) = fs::read_dir(accounts_dir(root)) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if let Some(id) = entry.file_name().to_str() {
                let _ = load_workouts(root, id);
            }
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        if f.read_exact(&mut out).is_ok() {
            return out;
        }
    }
    // 极端兜底：/dev/urandom 不可用时至少不产生空 token。
    let seed = format!("{}:{}:{}", now(), std::process::id(), out.len());
    for (i, b) in out.iter_mut().enumerate() {
        *b = sha256(format!("{seed}:{i}").as_bytes())[i % 32];
    }
    out
}

fn derive(password: &str, salt: &[u8]) -> [u8; 32] {
    let mut h = sha256(&[salt, password.as_bytes()].concat());
    for i in 0..PASSWORD_ROUNDS {
        let mut input = Vec::with_capacity(32 + salt.len() + password.len() + 8);
        input.extend_from_slice(&h);
        input.extend_from_slice(salt);
        input.extend_from_slice(password.as_bytes());
        input.extend_from_slice(&(i as u64).to_be_bytes());
        h = sha256(&input);
    }
    h
}

fn account_id(email: &str) -> String {
    hex(&sha256(email.to_ascii_lowercase().as_bytes()))[..24].to_string()
}

fn account_path(root: &Path, id: &str) -> PathBuf {
    root.join("accounts").join(format!("{id}.json"))
}

fn data_path(root: &Path, id: &str) -> PathBuf {
    root.join("accounts").join(id).join("health.json")
}

fn workouts_path(root: &Path, id: &str) -> PathBuf {
    root.join("accounts").join(id).join("workouts.json")
}

fn patches_path(root: &Path, id: &str) -> PathBuf {
    root.join("accounts").join(id).join("health.patches.jsonl")
}

fn app_config_path(root: &Path) -> PathBuf {
    root.join("app").join("config.json")
}

/// 旧客户端上传的是 gzip 快照；合并/读取运动索引前先解压。
fn decode_snapshot(raw: Vec<u8>) -> Option<Vec<u8>> {
    if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        GzDecoder::new(&raw[..]).read_to_end(&mut out).ok()?;
        Some(out)
    } else {
        Some(raw)
    }
}

fn key_of(row: &[Json], key_index: &[usize]) -> Option<String> {
    let mut values = Vec::with_capacity(key_index.len());
    for index in key_index {
        values.push(row.get(*index)?.clone());
    }
    Some(Json::Array(values).render())
}

/// 把一个增量块合并进当前快照。客户端按主键回表取完整行，所以这里
/// 只需要按 key 做替换/追加，删除则按 key 移除。
fn apply_delta(base: &mut Json, delta: &Json) -> Result<usize, String> {
    let delta_tables = delta
        .get("tables")
        .and_then(Json::as_object)
        .ok_or("missing tables")?;
    let base_object = base.as_object_mut().ok_or("base snapshot is not object")?;
    if base_object.iter().all(|(key, _)| key != "tables") {
        base_object.push(("tables".into(), Json::Object(Vec::new())));
    }
    let base_tables = base_object
        .iter_mut()
        .find(|(key, _)| key == "tables")
        .map(|(_, value)| value)
        .ok_or("missing base tables")?;
    let base_tables = base_tables
        .as_object_mut()
        .ok_or("base tables is not object")?;

    let mut applied = 0usize;
    for (table, block) in delta_tables {
        let key_names: Vec<&str> = block
            .get("key")
            .and_then(Json::as_array)
            .ok_or("missing key")?
            .iter()
            .filter_map(Json::as_str)
            .collect();
        let columns: Vec<Json> = block
            .get("columns")
            .and_then(Json::as_array)
            .ok_or("missing columns")?
            .to_vec();
        if key_names.is_empty() || columns.is_empty() {
            return Err("empty key/columns".into());
        }
        let column_names: Vec<&str> = columns.iter().filter_map(Json::as_str).collect();
        if column_names.len() != columns.len() {
            return Err("invalid columns".into());
        }
        let key_index = key_names
            .iter()
            .map(|name| column_names.iter().position(|c| c == name))
            .collect::<Option<Vec<_>>>()
            .ok_or("key not in columns")?;

        if base_tables.iter().all(|(name, _)| name != table) {
            base_tables.push((
                table.clone(),
                Json::Object(vec![
                    ("columns".into(), Json::Array(columns.clone())),
                    ("rows".into(), Json::Array(Vec::new())),
                ]),
            ));
        }
        let table_block = base_tables
            .iter_mut()
            .find(|(name, _)| name == table)
            .map(|(_, value)| value)
            .ok_or("table missing")?;
        let table_object = table_block.as_object_mut().ok_or("table not object")?;
        let existing_columns: Vec<&str> = table_object
            .iter()
            .find(|(name, _)| name == "columns")
            .and_then(|(_, value)| value.as_array())
            .ok_or("missing base columns")?
            .iter()
            .filter_map(Json::as_str)
            .collect();
        if existing_columns.len() != column_names.len()
            || existing_columns
                .iter()
                .zip(column_names.iter())
                .any(|(a, b)| a != b)
        {
            return Err("columns mismatch, need full snapshot".into());
        }
        let rows = table_object
            .iter_mut()
            .find(|(name, _)| name == "rows")
            .map(|(_, value)| value)
            .ok_or("missing base rows")?;
        let old_rows = match rows {
            Json::Array(items) => std::mem::take(items),
            _ => return Err("base rows not array".into()),
        };

        let deleted: std::collections::HashSet<String> = block
            .get("deletes")
            .and_then(Json::as_array)
            .unwrap_or(&[])
            .iter()
            .filter_map(Json::as_array)
            .filter_map(|values| key_of(values, &key_index))
            .collect();
        let mut replacements: HashMap<String, Json> = HashMap::new();
        for row in block.get("rows").and_then(Json::as_array).unwrap_or(&[]) {
            let Some(values) = row.as_array() else {
                continue;
            };
            if values.len() < column_names.len() {
                continue;
            }
            if let Some(key) = key_of(values, &key_index) {
                replacements.insert(key, row.clone());
            }
        }

        let mut merged = Vec::with_capacity(old_rows.len() + replacements.len());
        for row in old_rows {
            let Some(values) = row.as_array() else {
                merged.push(row);
                continue;
            };
            let Some(key) = key_of(values, &key_index) else {
                merged.push(row);
                continue;
            };
            if deleted.contains(&key) {
                applied += 1;
                continue;
            }
            if let Some(replacement) = replacements.remove(&key) {
                merged.push(replacement);
                applied += 1;
            } else {
                merged.push(row);
            }
        }
        // 剩下的都是新行。
        merged.extend(replacements.into_values());
        applied += 1;
        *rows = Json::Array(merged);
    }
    if let Some(exported) = base
        .as_object_mut()
        .and_then(|items| items.iter_mut().find(|(key, _)| key == "exported_ms"))
        .map(|(_, value)| value)
    {
        *exported = Json::Number(now() as f64 * 1000.0);
    }
    Ok(applied)
}

fn accounts_dir(root: &Path) -> PathBuf {
    root.join("accounts")
}

fn sessions_path(root: &Path) -> PathBuf {
    accounts_dir(root).join("sessions")
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 足够用的扁平 JSON 字符串取值器：账号请求只有两个字符串字段。
fn json_string(body: &[u8], key: &str) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let needle = format!("\"{key}\"");
    let mut from = 0usize;
    while let Some(pos) = text[from..].find(&needle) {
        let mut i = from + pos + needle.len();
        let bytes = text.as_bytes();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b':' {
            from = i;
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'"' {
            return None;
        }
        i += 1;
        let mut out = String::new();
        while i < bytes.len() {
            let c = bytes[i] as char;
            if c == '"' {
                return Some(out);
            }
            if c == '\\' {
                i += 1;
                if i >= bytes.len() {
                    return None;
                }
                match bytes[i] as char {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        if i + 4 >= bytes.len() {
                            return None;
                        }
                        let code = u32::from_str_radix(&text[i + 1..i + 5], 16).ok()?;
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        i += 4;
                    }
                    other => out.push(other),
                }
                i += 1;
                continue;
            }
            out.push(c);
            i += 1;
        }
        return None;
    }
    None
}

fn read_account(root: &Path, id: &str) -> Option<(String, Vec<u8>, [u8; 32])> {
    let body = fs::read(account_path(root, id)).ok()?;
    let email = json_string(&body, "email")?;
    let salt = unhex(&json_string(&body, "salt")?)?;
    let hash_bytes = unhex(&json_string(&body, "hash")?)?;
    if hash_bytes.len() != 32 {
        return None;
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&hash_bytes);
    Some((email, salt, hash))
}

fn write_account(
    root: &Path,
    id: &str,
    email: &str,
    salt: &[u8],
    hash: &[u8; 32],
) -> std::io::Result<()> {
    let dir = accounts_dir(root);
    fs::create_dir_all(&dir)?;
    let body = format!(
        "{{\"email\":\"{}\",\"salt\":\"{}\",\"hash\":\"{}\",\"created\":{}}}",
        json_escape(email),
        hex(salt),
        hex(hash),
        now()
    );
    fs::write(account_path(root, id), body)
}

fn load_sessions(root: &Path) -> Vec<(String, String, i64)> {
    let mut out = Vec::new();
    let Ok(text) = fs::read_to_string(sessions_path(root)) else {
        return out;
    };
    let current = now();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let (Some(token), Some(id), Some(exp)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(exp) = exp.parse::<i64>() else {
            continue;
        };
        if exp > current {
            out.push((token.to_string(), id.to_string(), exp));
        }
    }
    out
}

fn save_sessions(root: &Path, rows: &[(String, String, i64)]) -> std::io::Result<()> {
    let dir = accounts_dir(root);
    fs::create_dir_all(&dir)?;
    let body = rows
        .iter()
        .map(|(t, id, exp)| format!("{t}\t{id}\t{exp}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(sessions_path(root), body)
}

fn issue_session(root: &Path, id: &str) -> String {
    let _guard = SESSION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let token = hex(&random_bytes(32));
    let exp = now() + SESSION_SECONDS;
    let mut rows = load_sessions(root);
    rows.push((token.clone(), id.to_string(), exp));
    // 同一账号允许多个会话：手机、平板、桌面端登录互不踢下线。
    while rows.len() > 32 {
        rows.remove(0);
    }
    let _ = save_sessions(root, &rows);
    token
}

pub(crate) fn session_account(root: &Path, token: &str) -> Option<String> {
    let _guard = SESSION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let current = now();
    load_sessions(root)
        .into_iter()
        .find(|(t, _, exp)| eq_ct(t.as_bytes(), token.as_bytes()) && *exp > current)
        .map(|(_, id, _)| id)
}

/// 博客马拉松页只需要只读运动数据。给它一个长期静态 token，
/// 避免 30 天会话过期后公开页悄悄回退旧库。
fn blog_account(token: &str) -> Option<String> {
    let expected = std::env::var("VITALOOP_BLOG_TOKEN").ok()?;
    let id = std::env::var("VITALOOP_BLOG_ACCOUNT_ID").ok()?;
    blog_account_match(token, &expected, &id)
}

fn blog_account_match(token: &str, expected: &str, id: &str) -> Option<String> {
    if expected.is_empty() || !eq_ct(expected.as_bytes(), token.as_bytes()) {
        return None;
    }
    let id = id.trim();
    if id.is_empty() || id.contains('/') || id.contains('.') {
        return None;
    }
    Some(id.to_string())
}

pub(crate) fn bearer(req: &Request) -> Option<&str> {
    req.headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn json_reply(code: u16, body: String) -> (u16, &'static str, Vec<u8>) {
    (code, "application/json", body.into_bytes())
}

fn valid_email(email: &str) -> bool {
    let email = email.trim();
    email.len() >= 5
        && email.len() <= 200
        && email.contains('@')
        && email
            .rsplit_once('.')
            .map(|(_, tld)| !tld.is_empty())
            .unwrap_or(false)
}

fn register(root: &Path, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    let Some(email) = json_string(body, "email").map(|v| v.trim().to_ascii_lowercase()) else {
        return (400, "text/plain", b"missing email".to_vec());
    };
    let Some(password) = json_string(body, "password") else {
        return (400, "text/plain", b"missing password".to_vec());
    };
    if !valid_email(&email) {
        return (400, "text/plain", b"invalid email".to_vec());
    }
    if password.chars().count() < 8 {
        return (400, "text/plain", b"password too short".to_vec());
    }
    let id = account_id(&email);
    if account_path(root, &id).exists() {
        return (409, "text/plain", b"account exists".to_vec());
    }
    let salt = random_bytes(16);
    let hash = derive(&password, &salt);
    if let Err(e) = write_account(root, &id, &email, &salt, &hash) {
        return (
            500,
            "text/plain",
            format!("write account: {e}").into_bytes(),
        );
    }
    let token = issue_session(root, &id);
    json_reply(
        201,
        format!(
            "{{\"ok\":true,\"account_id\":\"{}\",\"email\":\"{}\",\"token\":\"{}\"}}",
            json_escape(&id),
            json_escape(&email),
            token
        ),
    )
}

fn login(root: &Path, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    let Some(email) = json_string(body, "email").map(|v| v.trim().to_ascii_lowercase()) else {
        return (400, "text/plain", b"missing email".to_vec());
    };
    let Some(password) = json_string(body, "password") else {
        return (400, "text/plain", b"missing password".to_vec());
    };
    if !valid_email(&email) {
        return (400, "text/plain", b"invalid email".to_vec());
    }
    let id = account_id(&email);
    let Some((stored_email, salt, expected)) = read_account(root, &id) else {
        return (401, "text/plain", b"bad credentials".to_vec());
    };
    if !eq_ct(stored_email.as_bytes(), email.as_bytes()) {
        return (401, "text/plain", b"bad credentials".to_vec());
    }
    let actual = derive(&password, &salt);
    if !eq_ct(&actual, &expected) {
        return (401, "text/plain", b"bad credentials".to_vec());
    }
    let token = issue_session(root, &id);
    json_reply(
        200,
        format!(
            "{{\"ok\":true,\"account_id\":\"{}\",\"email\":\"{}\",\"token\":\"{}\"}}",
            json_escape(&id),
            json_escape(&email),
            token
        ),
    )
}

pub fn route(req: &Request, root: &Path) -> Option<(u16, &'static str, Vec<u8>)> {
    let (path, query) = req
        .path
        .split_once('?')
        .map_or((req.path.as_str(), ""), |(p, q)| (p, q));
    match (req.method.as_str(), path) {
        ("GET", "/v1/app/config") | ("GET", "/api/v1/app/config") => Some(app_config(root)),
        ("POST", "/v1/account/register") => Some(register(root, &req.body)),
        ("POST", "/v1/account/login") => Some(login(root, &req.body)),
        ("GET", "/v1/account/me") => {
            let Some(token) = bearer(req) else {
                return Some((401, "text/plain", b"missing bearer".to_vec()));
            };
            let Some(id) = session_account(root, token) else {
                return Some((401, "text/plain", b"bad session".to_vec()));
            };
            let Some((email, _, _)) = read_account(root, &id) else {
                return Some((401, "text/plain", b"account missing".to_vec()));
            };
            Some(json_reply(
                200,
                format!(
                    "{{\"ok\":true,\"account_id\":\"{}\",\"email\":\"{}\"}}",
                    json_escape(&id),
                    json_escape(&email)
                ),
            ))
        }
        ("GET", "/v1/health/data") => {
            let Some(token) = bearer(req) else {
                return Some((401, "text/plain", b"missing bearer".to_vec()));
            };
            let Some(id) = session_account(root, token) else {
                return Some((401, "text/plain", b"bad session".to_vec()));
            };
            let base = fs::read(data_path(root, &id)).unwrap_or_else(|_| b"{}".to_vec());
            let patches = fs::read(patches_path(root, &id)).unwrap_or_default();
            let body = if patches.is_empty() {
                base
            } else {
                let Some(raw) = decode_snapshot(base) else {
                    return Some((500, "text/plain", b"cannot decode snapshot".to_vec()));
                };
                let Some(mut merged) = std::str::from_utf8(&raw)
                    .ok()
                    .and_then(|text| json::parse(text).ok())
                else {
                    return Some((500, "text/plain", b"invalid base snapshot".to_vec()));
                };
                for line in patches.split(|b| *b == b'\n') {
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(delta) = std::str::from_utf8(line)
                        .map_err(|_| ())
                        .and_then(|text| json::parse(text).map_err(|_| ()))
                    else {
                        continue;
                    };
                    if apply_delta(&mut merged, &delta).is_err() {
                        return Some((500, "text/plain", b"invalid delta".to_vec()));
                    }
                }
                merged.render().into_bytes()
            };
            Some((200, "application/json", body))
        }
        ("PUT", "/v1/health/data") => {
            let Some(token) = bearer(req) else {
                return Some((401, "text/plain", b"missing bearer".to_vec()));
            };
            let Some(id) = session_account(root, token) else {
                return Some((401, "text/plain", b"bad session".to_vec()));
            };
            let file = data_path(root, &id);
            if let Some(parent) = file.parent() {
                let _ = fs::create_dir_all(parent);
            }
            // 先写临时文件再原子改名：进程被杀/磁盘满时旧快照仍然完整。
            let tmp = file.with_extension("json.tmp");
            let result = fs::write(&tmp, &req.body).and_then(|_| fs::rename(&tmp, &file));
            match result {
                Ok(_) => {
                    // 全量快照已经包含所有增量，旧补丁必须清空。
                    let _ = fs::write(patches_path(root, &id), b"");
                    Some(json_reply(200, "{\"ok\":true}".to_string()))
                }
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    Some((500, "text/plain", format!("write health: {e}").into_bytes()))
                }
            }
        }
        ("PUT", "/v1/health/delta") => {
            let Some(token) = bearer(req) else {
                return Some((401, "text/plain", b"missing bearer".to_vec()));
            };
            let Some(id) = session_account(root, token) else {
                return Some((401, "text/plain", b"bad session".to_vec()));
            };
            let base = data_path(root, &id);
            if !base.exists() {
                // 没有全量基线时增量无法落地，让客户端先传一次完整快照。
                return Some((
                    409,
                    "text/plain",
                    b"no base snapshot, send full snapshot first".to_vec(),
                ));
            }
            let Ok(text) = std::str::from_utf8(&req.body) else {
                return Some((400, "text/plain", b"invalid delta utf8".to_vec()));
            };
            let Ok(value) = json::parse(text) else {
                return Some((400, "text/plain", b"invalid delta json".to_vec()));
            };
            if value.get("tables").and_then(Json::as_object).is_none() {
                return Some((400, "text/plain", b"missing delta tables".to_vec()));
            }
            let patches = patches_path(root, &id);
            let current_size = fs::metadata(&patches).map(|m| m.len()).unwrap_or(0);
            if current_size > 8 * 1024 * 1024 {
                // 补丁链太长会让登录下载变慢，要求客户端重新做一次全量压缩。
                return Some((
                    409,
                    "text/plain",
                    b"patch log too large, send full snapshot".to_vec(),
                ));
            }
            let mut line = text.trim().to_string();
            line.push('\n');
            if let Some(parent) = patches.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let result = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&patches)
                .and_then(|mut file| {
                    use std::io::Write;
                    file.write_all(line.as_bytes())
                });
            match result {
                Ok(_) => Some(json_reply(200, "{\"ok\":true}".to_string())),
                Err(e) => Some((500, "text/plain", format!("append delta: {e}").into_bytes())),
            }
        }
        ("PUT", "/v1/health/workouts") => {
            let Some(token) = bearer(req) else {
                return Some((401, "text/plain", b"missing bearer".to_vec()));
            };
            let Some(id) = session_account(root, token) else {
                return Some((401, "text/plain", b"bad session".to_vec()));
            };
            let Ok(value) = json::parse(std::str::from_utf8(&req.body).unwrap_or("")) else {
                return Some((400, "text/plain", b"invalid workouts json".to_vec()));
            };
            if value.get("columns").and_then(Json::as_array).is_none()
                || value.get("rows").and_then(Json::as_array).is_none()
            {
                return Some((400, "text/plain", b"expected columns/rows".to_vec()));
            }
            let file = workouts_path(root, &id);
            if let Some(parent) = file.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let tmp = file.with_extension("json.tmp");
            match fs::write(&tmp, &req.body).and_then(|_| fs::rename(&tmp, &file)) {
                Ok(_) => Some(json_reply(200, "{\"ok\":true}".to_string())),
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    Some((
                        500,
                        "text/plain",
                        format!("write workouts: {e}").into_bytes(),
                    ))
                }
            }
        }
        ("GET", "/v1/workouts") => {
            let Some(token) = bearer(req) else {
                return Some((401, "text/plain", b"missing bearer".to_vec()));
            };
            let Some(id) = session_account(root, token).or_else(|| blog_account(token)) else {
                return Some((401, "text/plain", b"bad session".to_vec()));
            };
            let limit = query
                .split('&')
                .find_map(|part| part.strip_prefix("limit="))
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(50)
                .clamp(1, 500);
            let marathon_only = query.split('&').any(|part| part == "marathon=1");
            let workouts = load_workouts(root, &id).unwrap_or_else(|| Arc::new(Vec::new()));
            let mut ordered: Vec<&Json> = workouts
                .iter()
                .filter(|workout| !marathon_only || is_marathon_workout(workout))
                .collect();
            ordered.sort_by(|a, b| {
                workout_start(b)
                    .partial_cmp(&workout_start(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let total = ordered.len();
            let selected: Vec<Json> = ordered.into_iter().take(limit).cloned().collect();
            Some(json_reply(
                200,
                Json::Object(vec![
                    ("ok".into(), Json::Bool(true)),
                    ("total".into(), Json::Number(total as f64)),
                    ("workouts".into(), Json::Array(selected)),
                ])
                .render(),
            ))
        }
        ("GET", path) if path.starts_with("/v1/workouts/") => {
            let Some(token) = bearer(req) else {
                return Some((401, "text/plain", b"missing bearer".to_vec()));
            };
            let Some(id) = session_account(root, token).or_else(|| blog_account(token)) else {
                return Some((401, "text/plain", b"bad session".to_vec()));
            };
            let wanted = &path["/v1/workouts/".len()..];
            let workouts = load_workouts(root, &id).unwrap_or_else(|| Arc::new(Vec::new()));
            let found = workouts
                .iter()
                .find(|workout| workout.get("id").and_then(Json::as_str) == Some(wanted))
                .cloned();
            match found {
                Some(workout) => Some(json_reply(
                    200,
                    Json::Object(vec![
                        ("ok".into(), Json::Bool(true)),
                        ("workout".into(), workout),
                    ])
                    .render(),
                )),
                None => Some(json_reply(
                    404,
                    "{\"ok\":false,\"error\":\"workout not found\"}".to_string(),
                )),
            }
        }
        _ => None,
    }
}

/// App 更新配置保持公开：客户端未登录也要能检查版本。配置只包含版本号、
/// 下载地址和更新说明，不涉及账号数据。文件不存在时返回空配置，客户端据此
/// 显示“已是最新版本”。
fn app_config(root: &Path) -> (u16, &'static str, Vec<u8>) {
    let raw = match fs::read(app_config_path(root)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return json_reply(200, r#"{"code":0,"data":{}}"#.to_string());
        }
        Err(e) => {
            return json_reply(
                500,
                format!(
                    r#"{{"code":1,"message":"read app config: {}"}}"#,
                    json_escape(&e.to_string())
                ),
            );
        }
    };
    let text = match std::str::from_utf8(&raw) {
        Ok(text) => text,
        Err(_) => {
            return json_reply(
                500,
                r#"{"code":1,"message":"app config is not utf-8"}"#.to_string(),
            );
        }
    };
    let value = match json::parse(text) {
        Ok(value) => value,
        Err(e) => {
            return json_reply(
                500,
                format!(
                    r#"{{"code":1,"message":"invalid app config: {}"}}"#,
                    json_escape(&e)
                ),
            );
        }
    };
    match value {
        Json::Object(_) => json_reply(
            200,
            Json::Object(vec![
                ("code".into(), Json::Number(0.0)),
                ("data".into(), value),
            ])
            .render(),
        ),
        _ => json_reply(
            500,
            r#"{"code":1,"message":"app config must be an object"}"#.to_string(),
        ),
    }
}

fn workout_start(workout: &Json) -> f64 {
    workout
        .get("start_ms")
        .or_else(|| workout.get("startMs"))
        .and_then(Json::as_f64)
        .unwrap_or(0.0)
}

fn is_marathon_workout(workout: &Json) -> bool {
    let sport = workout
        .get("sport")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let distance = workout
        .get("distance_m")
        .or_else(|| workout.get("distanceM"))
        .and_then(Json::as_f64)
        .unwrap_or(0.0);
    (sport.contains("run") || sport.contains("jog"))
        && ((20_000.0..=23_000.0).contains(&distance) || (40_000.0..=45_000.0).contains(&distance))
}

/// 读取客户端随快照上传的运动表格；旧客户端只有 health.json 时也能读明文快照。
fn file_stamp(path: &Path) -> FileStamp {
    let metadata = fs::metadata(path).ok();
    FileStamp {
        modified: metadata.as_ref().and_then(|m| m.modified().ok()),
        len: metadata.map(|m| m.len()).unwrap_or(0),
    }
}

pub(crate) fn load_workouts(root: &Path, id: &str) -> Option<Arc<Vec<Json>>> {
    let health = file_stamp(&data_path(root, id));
    let workouts = file_stamp(&workouts_path(root, id));
    let key = data_path(root, id);
    let cache = WORKOUT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(entries) = cache.lock() {
        if let Some(entry) = entries.get(&key) {
            // shortcut: 健康补丁不使运动缓存失效；运动更新走 /v1/health/workouts，
            // 若以后客户端改为在 health delta 里更新 workouts，需要改为增量识别。
            if entry.health == health && entry.workouts == workouts {
                return Some(entry.rows.clone());
            }
        }
    }

    // 同一账号首次读取只解析一次，避免并发请求同时复制、解析整份 health 快照。
    let _load_guard = WORKOUT_LOAD_LOCK.lock().ok()?;
    if let Ok(entries) = cache.lock() {
        if let Some(entry) = entries.get(&key) {
            if entry.health == health && entry.workouts == workouts {
                return Some(entry.rows.clone());
            }
        }
    }

    let value = merged_health(root, id).or_else(|| {
        let raw = fs::read(workouts_path(root, id)).ok()?;
        json::parse(std::str::from_utf8(&raw).ok()?).ok()
    })?;
    let rows = Arc::new(workout_rows(value)?);
    if let Ok(mut entries) = cache.lock() {
        entries.insert(
            key,
            WorkoutCacheEntry {
                health,
                workouts,
                rows: rows.clone(),
            },
        );
    }
    Some(rows)
}

fn workout_rows(value: Json) -> Option<Vec<Json>> {
    let table = if value.get("tables").is_some() {
        value.get("tables")?.get("workouts")?
    } else {
        &value
    };
    let columns: Vec<&str> = table
        .get("columns")?
        .as_array()?
        .iter()
        .filter_map(Json::as_str)
        .collect();
    let rows = table.get("rows")?.as_array()?;
    let mut out = Vec::with_capacity(rows.len());
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
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect();
        out.push(Json::Object(fields));
    }
    Some(out)
}

/// 读取基线快照并依次套用增量补丁。补丁为空时直接返回基线，避免每次
/// 拉运动索引都重新解析几十兆 JSON。
pub(crate) fn merged_health(root: &Path, id: &str) -> Option<Json> {
    let raw = fs::read(data_path(root, id)).ok()?;
    let patches = fs::read(patches_path(root, id)).unwrap_or_default();
    let decoded = decode_snapshot(raw)?;
    let mut value = json::parse(std::str::from_utf8(&decoded).ok()?).ok()?;
    for line in patches.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(delta) = std::str::from_utf8(line)
            .map_err(|_| ())
            .and_then(|text| json::parse(text).map_err(|_| ()))
        else {
            continue;
        };
        let _ = apply_delta(&mut value, &delta);
    }
    Some(value)
}

/// 只合并指定表，避免公开博客接口为无关表反复复制整份健康快照。
pub(crate) fn merged_health_tables(root: &Path, id: &str, names: &[&str]) -> Option<Json> {
    let raw = fs::read(data_path(root, id)).ok()?;
    let patches = fs::read(patches_path(root, id)).unwrap_or_default();
    let decoded = decode_snapshot(raw)?;
    let mut value = json::parse(std::str::from_utf8(&decoded).ok()?).ok()?;
    retain_tables(&mut value, names);
    for line in patches.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(delta) = std::str::from_utf8(line)
            .map_err(|_| ())
            .and_then(|text| json::parse(text).map_err(|_| ()))
        else {
            continue;
        };
        let Some(delta) = select_tables(&delta, names) else {
            continue;
        };
        let _ = apply_delta(&mut value, &delta);
    }
    Some(value)
}

fn retain_tables(snapshot: &mut Json, names: &[&str]) {
    let Some(tables) = snapshot
        .as_object_mut()
        .and_then(|items| items.iter_mut().find(|(key, _)| key == "tables"))
        .map(|(_, value)| value)
    else {
        return;
    };
    if let Some(items) = tables.as_object_mut() {
        items.retain(|(name, _)| names.contains(&name.as_str()));
    }
}

fn select_tables(delta: &Json, names: &[&str]) -> Option<Json> {
    let tables = delta.get("tables")?.as_object()?;
    let selected: Vec<(String, Json)> = tables
        .iter()
        .filter(|(name, _)| names.contains(&name.as_str()))
        .cloned()
        .collect();
    if selected.is_empty() {
        return None;
    }
    Some(Json::Object(vec![(
        "tables".into(),
        Json::Object(selected),
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "awen-sync-account-test-{}-{}-{}",
            std::process::id(),
            now(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn json_string_handles_escapes() {
        let body = br#"{"email":"a@b.co","password":"p\"ass\\x"}"#;
        assert_eq!(json_string(body, "email").as_deref(), Some("a@b.co"));
        assert_eq!(json_string(body, "password").as_deref(), Some("p\"ass\\x"));
    }

    #[test]
    fn account_id_is_stable_and_email_is_case_insensitive() {
        assert_eq!(account_id("Hi@Example.com"), account_id("hi@example.com"));
        assert_ne!(account_id("a@example.com"), account_id("b@example.com"));
    }

    #[test]
    fn blog_token_maps_to_one_read_only_account() {
        let id = "0123456789abcdef01234567";
        assert_eq!(
            blog_account_match("long-random-blog-token", "long-random-blog-token", id).as_deref(),
            Some(id)
        );
        assert_eq!(
            blog_account_match("wrong-token", "long-random-blog-token", id),
            None
        );
        assert_eq!(blog_account_match("x", "", id), None);
        assert_eq!(
            blog_account_match("x", "x", "../other-account"),
            None,
            "账号目录不允许路径穿越"
        );
    }

    #[test]
    fn password_verifier_is_salted() {
        let a = derive("correct horse", b"0123456789abcdef");
        let b = derive("correct horse", b"fedcba9876543210");
        assert_ne!(a, b);
        assert_eq!(a, derive("correct horse", b"0123456789abcdef"));
    }

    #[test]
    fn register_login_and_health_snapshot_roundtrip() {
        let root = dir();
        let register = Request {
            method: "POST".into(),
            path: "/v1/account/register".into(),
            headers: HashMap::new(),
            body: br#"{"email":"hi@example.com","password":"test-password-123"}"#.to_vec(),
        };
        let (code, _, body) = route(&register, &root).unwrap();
        assert_eq!(code, 201);
        let token = json_string(&body, "token").unwrap();

        let login = Request {
            method: "POST".into(),
            path: "/v1/account/login".into(),
            headers: HashMap::new(),
            body: br#"{"email":"HI@example.com","password":"test-password-123"}"#.to_vec(),
        };
        let (code, _, body) = route(&login, &root).unwrap();
        assert_eq!(code, 200);
        let login_token = json_string(&body, "token").unwrap();

        let mut headers = HashMap::new();
        headers.insert("authorization".into(), format!("Bearer {login_token}"));
        let put = Request {
            method: "PUT".into(),
            path: "/v1/health/data".into(),
            headers: headers.clone(),
            body: br#"{"version":1,"tables":{}}"#.to_vec(),
        };
        assert_eq!(route(&put, &root).unwrap().0, 200);
        let get = Request {
            method: "GET".into(),
            path: "/v1/health/data".into(),
            headers,
            body: Vec::new(),
        };
        let (code, _, body) = route(&get, &root).unwrap();
        assert_eq!(code, 200);
        assert_eq!(body, br#"{"version":1,"tables":{}}"#);
        // 第二次登录不能把第一次的会话踢下线（多设备同时同步）。
        let mut first_headers = HashMap::new();
        first_headers.insert("authorization".into(), format!("Bearer {token}"));
        let me = Request {
            method: "GET".into(),
            path: "/v1/account/me".into(),
            headers: first_headers,
            body: Vec::new(),
        };
        assert_eq!(route(&me, &root).unwrap().0, 200);
        assert!(!token.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workouts_are_listed_and_fetched_by_id() {
        let root = dir();
        let register = Request {
            method: "POST".into(),
            path: "/v1/account/register".into(),
            headers: HashMap::new(),
            body: br#"{"email":"workout@example.com","password":"test-password-123"}"#.to_vec(),
        };
        let (_, _, body) = route(&register, &root).unwrap();
        let token = json_string(&body, "token").unwrap();
        let mut headers = HashMap::new();
        headers.insert("authorization".into(), format!("Bearer {token}"));

        let block = br#"{"columns":["id","sport","start_ms","distance_m"],"rows":[["w-old","outdoor_run",1000,5.0],["w-new","cycling",2000,12.5],["w-mar","running",1500,21097.5]]}"#;
        let put = Request {
            method: "PUT".into(),
            path: "/v1/health/workouts".into(),
            headers: headers.clone(),
            body: block.to_vec(),
        };
        assert_eq!(route(&put, &root).unwrap().0, 200);

        let list = Request {
            method: "GET".into(),
            path: "/v1/workouts?limit=1".into(),
            headers: headers.clone(),
            body: Vec::new(),
        };
        let (code, _, body) = route(&list, &root).unwrap();
        assert_eq!(code, 200);
        let parsed = json::parse(std::str::from_utf8(&body).unwrap()).unwrap();
        assert_eq!(parsed.get("total").and_then(Json::as_f64), Some(3.0));
        let workouts = parsed.get("workouts").and_then(Json::as_array).unwrap();
        assert_eq!(workouts.len(), 1);
        assert_eq!(workouts[0].get("id").and_then(Json::as_str), Some("w-new"));

        let marathon = Request {
            method: "GET".into(),
            path: "/v1/workouts?limit=500&marathon=1".into(),
            headers: headers.clone(),
            body: Vec::new(),
        };
        let (code, _, body) = route(&marathon, &root).unwrap();
        assert_eq!(code, 200);
        let parsed = json::parse(std::str::from_utf8(&body).unwrap()).unwrap();
        assert_eq!(parsed.get("total").and_then(Json::as_f64), Some(1.0));
        let workouts = parsed.get("workouts").and_then(Json::as_array).unwrap();
        assert_eq!(workouts[0].get("id").and_then(Json::as_str), Some("w-mar"));

        let detail = Request {
            method: "GET".into(),
            path: "/v1/workouts/w-old".into(),
            headers,
            body: Vec::new(),
        };
        let (code, _, body) = route(&detail, &root).unwrap();
        assert_eq!(code, 200);
        let parsed = json::parse(std::str::from_utf8(&body).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("workout")
                .and_then(|w| w.get("sport"))
                .and_then(Json::as_str),
            Some("outdoor_run")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn app_config_is_public_and_uses_legacy_envelope() {
        let root = dir();
        let config_dir = root.join("app");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.json"),
            br#"{"latest_version":"1.2.3","latest_version_code":12,"download_url":"https://example.com/vitaloop.apk","update_notes":"test","force_update":false}"#,
        )
        .unwrap();

        for path in ["/v1/app/config", "/api/v1/app/config"] {
            let req = Request {
                method: "GET".into(),
                path: path.into(),
                headers: HashMap::new(),
                body: Vec::new(),
            };
            let (code, ctype, body) = route(&req, &root).unwrap();
            assert_eq!(code, 200);
            assert_eq!(ctype, "application/json");
            let parsed = json::parse(std::str::from_utf8(&body).unwrap()).unwrap();
            assert_eq!(parsed.get("code").and_then(Json::as_f64), Some(0.0));
            let data = parsed.get("data").unwrap();
            assert_eq!(
                data.get("latest_version").and_then(Json::as_str),
                Some("1.2.3")
            );
            assert_eq!(
                data.get("latest_version_code").and_then(Json::as_f64),
                Some(12.0)
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_app_config_is_an_empty_public_response() {
        let root = dir();
        let req = Request {
            method: "GET".into(),
            path: "/v1/app/config".into(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (code, _, body) = route(&req, &root).unwrap();
        assert_eq!(code, 200);
        let parsed = json::parse(std::str::from_utf8(&body).unwrap()).unwrap();
        assert_eq!(parsed.get("code").and_then(Json::as_f64), Some(0.0));
        assert!(matches!(parsed.get("data"), Some(Json::Object(items)) if items.is_empty()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delta_merges_upserts_and_deletes_by_key() {
        let mut base = json::parse(
            r#"{"version":1,"tables":{"heart_rate_points":{"columns":["ts","bpm"],"rows":[[1,70],[2,71]]}}}"#,
        )
        .unwrap();
        let delta = json::parse(
            r#"{"version":2,"tables":{"heart_rate_points":{
                "key":["ts"],"columns":["ts","bpm"],
                "rows":[[1,75],[3,80]],"deletes":[[2]]}}}"#,
        )
        .unwrap();
        apply_delta(&mut base, &delta).unwrap();
        let rows = base
            .get("tables")
            .and_then(|v| v.get("heart_rate_points"))
            .and_then(|v| v.get("rows"))
            .and_then(Json::as_array)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].as_array().unwrap()[1].as_f64(), Some(75.0));
        assert_eq!(rows[1].as_array().unwrap()[0].as_f64(), Some(3.0));
    }

    #[test]
    fn table_scoped_merge_keeps_only_requested_tables() {
        let root = dir();
        let id = "account";
        let account_dir = root.join("accounts").join(id);
        fs::create_dir_all(&account_dir).unwrap();
        fs::write(
            account_dir.join("health.json"),
            br#"{"tables":{"daily_metrics":{"columns":["date_day","steps"],"rows":[[1,100]]},"sleep":{"columns":["date_day"],"rows":[[1]]}}}"#,
        )
        .unwrap();
        fs::write(
            account_dir.join("health.patches.jsonl"),
            br#"{"tables":{"daily_metrics":{"key":["date_day"],"columns":["date_day","steps"],"rows":[[2,200]]},"sleep":{"key":["date_day"],"columns":["date_day"],"deletes":[[1]]}}}
"#,
        )
        .unwrap();

        let merged = merged_health_tables(&root, id, &["daily_metrics"]).expect("merged snapshot");
        let tables = merged.get("tables").and_then(Json::as_object).unwrap();
        assert_eq!(tables.len(), 1);
        let rows = merged
            .get("tables")
            .and_then(|value| value.get("daily_metrics"))
            .and_then(|value| value.get("rows"))
            .and_then(Json::as_array)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].as_array().unwrap()[1].as_f64(), Some(200.0));
        let _ = fs::remove_dir_all(root);
    }
}
