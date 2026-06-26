#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn object() -> Json {
        Json::Object(Vec::new())
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Object(items) => items.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Json::String(s)) => Some(s),
            _ => None,
        }
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key) {
            Some(Json::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    pub fn set(&mut self, key: &str, value: Json) {
        if let Json::Object(items) = self {
            if let Some((_, existing)) = items.iter_mut().find(|(k, _)| k == key) {
                *existing = value;
            } else {
                items.push((key.to_string(), value));
            }
        }
    }

    pub fn remove(&mut self, key: &str) {
        if let Json::Object(items) = self {
            items.retain(|(k, _)| k != key);
        }
    }

    pub fn stringify(&self) -> String {
        match self {
            Json::Null => "null".to_string(),
            Json::Bool(v) => v.to_string(),
            Json::Number(n) => n.clone(),
            Json::String(s) => format!("\"{}\"", escape(s)),
            Json::Array(values) => {
                let inner = values
                    .iter()
                    .map(|v| v.stringify())
                    .collect::<Vec<_>>()
                    .join(",");
                format!("[{inner}]")
            }
            Json::Object(items) => {
                let inner = items
                    .iter()
                    .map(|(k, v)| format!("\"{}\":{}", escape(k), v.stringify()))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{{{inner}}}")
            }
        }
    }
}

impl From<&str> for Json {
    fn from(value: &str) -> Self {
        Json::String(value.to_string())
    }
}

impl From<String> for Json {
    fn from(value: String) -> Self {
        Json::String(value)
    }
}

impl From<bool> for Json {
    fn from(value: bool) -> Self {
        Json::Bool(value)
    }
}

pub fn parse(input: &str) -> Result<Json, String> {
    let mut parser = Parser { input, pos: 0 };
    let value = parser.value()?;
    parser.ws();
    if parser.pos != input.len() {
        return Err(format!("unexpected trailing JSON at byte {}", parser.pos));
    }
    Ok(value)
}

pub fn escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.peek() {
            Some(b'n') => self.literal("null", Json::Null),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'"') => self.string().map(Json::String),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(other) => Err(format!("unexpected JSON byte {other} at {}", self.pos)),
            None => Err("unexpected end of JSON".to_string()),
        }
    }

    fn literal(&mut self, text: &str, value: Json) -> Result<Json, String> {
        if self.input[self.pos..].starts_with(text) {
            self.pos += text.len();
            Ok(value)
        } else {
            Err(format!("expected {text} at byte {}", self.pos))
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut values = Vec::new();
        loop {
            self.ws();
            if self.consume(b']') {
                break;
            }
            values.push(self.value()?);
            self.ws();
            if self.consume(b']') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(Json::Array(values))
    }

    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut items = Vec::new();
        loop {
            self.ws();
            if self.consume(b'}') {
                break;
            }
            let key = self.string()?;
            self.ws();
            self.expect(b':')?;
            let value = self.value()?;
            items.push((key, value));
            self.ws();
            if self.consume(b'}') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(Json::Object(items))
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        self.consume(b'-');
        self.take_digits();
        if self.consume(b'.') {
            self.take_digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            self.take_digits();
        }
        Ok(Json::Number(self.input[start..self.pos].to_string()))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        while let Some(ch) = self.next_char() {
            match ch {
                '"' => return Ok(out),
                '\\' => {
                    let escaped = self
                        .next_char()
                        .ok_or_else(|| "unterminated JSON escape".to_string())?;
                    match escaped {
                        '"' | '\\' | '/' => out.push(escaped),
                        'b' => out.push('\u{08}'),
                        'f' => out.push('\u{0c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let code = self.take_hex4()?;
                            if let Some(c) = char::from_u32(code) {
                                out.push(c);
                            }
                        }
                        other => return Err(format!("invalid JSON escape \\{other}")),
                    }
                }
                c => out.push(c),
            }
        }
        Err("unterminated JSON string".to_string())
    }

    fn take_hex4(&mut self) -> Result<u32, String> {
        if self.pos + 4 > self.input.len() {
            return Err("short unicode escape".to_string());
        }
        let text = &self.input[self.pos..self.pos + 4];
        self.pos += 4;
        u32::from_str_radix(text, 16).map_err(|_| "invalid unicode escape".to_string())
    }

    fn take_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.consume(byte) {
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", byte as char, self.pos))
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn next_char(&mut self) -> Option<char> {
        let ch = self.input[self.pos..].chars().next()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }
}
