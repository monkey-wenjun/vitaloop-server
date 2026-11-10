//! 够用的小型 JSON 解析器。
//!
//! 服务端是 scratch 容器、零第三方 crate，账号快照里的 workouts 表格又不值得
//! 引入 serde。这里只实现标准 JSON 的读取与序列化，够解析 health 快照并稳定输出。

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(items) => items
                .iter()
                .find_map(|(k, v)| if k == key { Some(v) } else { None }),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Vec<(String, Json)>> {
        match self {
            Json::Object(items) => Some(items),
            _ => None,
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Number(value) => {
                if value.is_finite() {
                    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0 {
                        out.push_str(&format!("{}", *value as i64));
                    } else {
                        out.push_str(&format!("{value}"));
                    }
                } else {
                    out.push_str("null");
                }
            }
            Json::String(value) => write_string(value, out),
            Json::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(items) => {
                out.push('{');
                for (index, (key, value)) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_string(value: &str, out: &mut String) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if c < '\u{20}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    src: &'a [u8],
    at: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            src: text.as_bytes(),
            at: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.at < self.src.len() && self.src[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        let Some(&byte) = self.src.get(self.at) else {
            return Err("unexpected end".into());
        };
        match byte {
            b'n' => {
                self.keyword(b"null")?;
                Ok(Json::Null)
            }
            b't' => {
                self.keyword(b"true")?;
                Ok(Json::Bool(true))
            }
            b'f' => {
                self.keyword(b"false")?;
                Ok(Json::Bool(false))
            }
            b'"' => Ok(Json::String(self.string()?)),
            b'[' => self.array(),
            b'{' => self.object(),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(format!("unexpected byte {byte:#x} at {}", self.at)),
        }
    }

    fn keyword(&mut self, word: &[u8]) -> Result<(), String> {
        if self.src.get(self.at..self.at + word.len()) == Some(word) {
            self.at += word.len();
            Ok(())
        } else {
            Err(format!("expected keyword at {}", self.at))
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.at += 1; // opening quote
        let mut out = String::new();
        while self.at < self.src.len() {
            let byte = self.src[self.at];
            self.at += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&escaped) = self.src.get(self.at) else {
                        return Err("unterminated escape".into());
                    };
                    self.at += 1;
                    match escaped {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let cp = self.hex4()?;
                            if (0xD800..=0xDBFF).contains(&cp) {
                                // 代理对：再读一个 \uXXXX。
                                if self.src.get(self.at..self.at + 2) != Some(b"\\u") {
                                    return Err("missing low surrogate".into());
                                }
                                self.at += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return Err("invalid low surrogate".into());
                                }
                                let scalar = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
                                out.push(char::from_u32(scalar).ok_or("invalid scalar")?);
                            } else {
                                out.push(char::from_u32(cp).ok_or("invalid scalar")?);
                            }
                        }
                        _ => return Err("invalid escape".into()),
                    }
                }
                0x00..=0x1f => return Err("control character in string".into()),
                _ => {
                    // 非 ASCII 字节按 UTF-8 原样塞回去；源输入已经是 &str，边界有效。
                    let start = self.at - 1;
                    let width = utf8_width(byte);
                    let end = start + width;
                    if end > self.src.len() {
                        return Err("truncated utf-8".into());
                    }
                    let piece =
                        std::str::from_utf8(&self.src[start..end]).map_err(|_| "invalid utf-8")?;
                    out.push_str(piece);
                    self.at = end;
                }
            }
        }
        Err("unterminated string".into())
    }

    fn hex4(&mut self) -> Result<u32, String> {
        if self.at + 4 > self.src.len() {
            return Err("truncated unicode escape".into());
        }
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self.src[self.at];
            self.at += 1;
            value = value * 16
                + match byte {
                    b'0'..=b'9' => (byte - b'0') as u32,
                    b'a'..=b'f' => (byte - b'a' + 10) as u32,
                    b'A'..=b'F' => (byte - b'A' + 10) as u32,
                    _ => return Err("invalid unicode escape".into()),
                };
        }
        Ok(value)
    }

    fn array(&mut self) -> Result<Json, String> {
        self.at += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.src.get(self.at) == Some(&b']') {
            self.at += 1;
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.src.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err("expected , or ] in array".into()),
            }
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.at += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.src.get(self.at) == Some(&b'}') {
            self.at += 1;
            return Ok(Json::Object(items));
        }
        loop {
            self.skip_ws();
            if self.src.get(self.at) != Some(&b'"') {
                return Err("expected object key".into());
            }
            let key = self.string()?;
            self.skip_ws();
            if self.src.get(self.at) != Some(&b':') {
                return Err("expected : after object key".into());
            }
            self.at += 1;
            items.push((key, self.value()?));
            self.skip_ws();
            match self.src.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Json::Object(items));
                }
                _ => return Err("expected , or } in object".into()),
            }
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.at;
        while let Some(&byte) = self.src.get(self.at) {
            match byte {
                b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9' => self.at += 1,
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.src[start..self.at]).map_err(|_| "invalid number")?;
        text.parse::<f64>()
            .map(Json::Number)
            .map_err(|_| "invalid number".into())
    }
}

fn utf8_width(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

pub fn parse(text: &str) -> Result<Json, String> {
    let mut parser = Parser::new(text);
    let value = parser.value()?;
    parser.skip_ws();
    if parser.at != parser.src.len() {
        return Err(format!("trailing data at {}", parser.at));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_snapshot() {
        let value = parse(
            r#"{"tables":{"workouts":{"columns":["id","start_ms","distance_m"],"rows":[["w1",123,4.5]]}}}"#,
        )
        .unwrap();
        let rows = value
            .get("tables")
            .and_then(|v| v.get("workouts"))
            .and_then(|v| v.get("rows"))
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(rows[0].as_array().unwrap()[0].as_str(), Some("w1"));
    }

    #[test]
    fn roundtrips_escaped_text() {
        let value = parse(r#"{"name":"a\"b\n中文","ok":true}"#).unwrap();
        let rendered = value.render();
        assert_eq!(parse(&rendered).unwrap(), value);
    }
}
