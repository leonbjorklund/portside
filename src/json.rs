//! A strict JSON reader, enough for GitHub's release response. It rejects
//! anything a careless parser would accept: trailing data, duplicate keys,
//! bad escapes, control characters and deep nesting.

#[derive(Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

const MAX_DEPTH: usize = 16;

pub fn parse(text: &str) -> Result<Json, &'static str> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        at: 0,
    };
    let value = reader.value(0)?;
    reader.space();
    if reader.at != reader.bytes.len() {
        return Err("data after the JSON value");
    }
    Ok(value)
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn bool(&self) -> Option<bool> {
        match self {
            Json::Bool(flag) => Some(*flag),
            _ => None,
        }
    }

    /// A whole number a double holds exactly.
    pub fn u64(&self) -> Option<u64> {
        match self {
            Json::Number(n) if n.fract() == 0.0 && (0.0..=9007199254740992.0).contains(n) => {
                Some(*n as u64)
            }
            _ => None,
        }
    }

    pub fn array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn space(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.bytes.get(self.at) == Some(&byte);
        self.at += usize::from(found);
        found
    }

    fn word(&mut self, word: &str, value: Json) -> Result<Json, &'static str> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err("unexpected text")
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, &'static str> {
        if depth > MAX_DEPTH {
            return Err("nested too deeply");
        }
        self.space();
        match self.bytes.get(self.at) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Json::String),
            Some(b't') => self.word("true", Json::Bool(true)),
            Some(b'f') => self.word("false", Json::Bool(false)),
            Some(b'n') => self.word("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err("unexpected text"),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, &'static str> {
        self.at += 1;
        let mut fields: Vec<(String, Json)> = Vec::new();
        self.space();
        if self.eat(b'}') {
            return Ok(Json::Object(fields));
        }
        loop {
            self.space();
            if self.bytes.get(self.at) != Some(&b'"') {
                return Err("expected a key");
            }
            let key = self.string()?;
            if fields.iter().any(|(k, _)| *k == key) {
                return Err("duplicate key");
            }
            self.space();
            if !self.eat(b':') {
                return Err("expected ':'");
            }
            fields.push((key, self.value(depth + 1)?));
            self.space();
            if self.eat(b'}') {
                return Ok(Json::Object(fields));
            }
            if !self.eat(b',') {
                return Err("expected ',' or '}'");
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, &'static str> {
        self.at += 1;
        let mut items = Vec::new();
        self.space();
        if self.eat(b']') {
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.space();
            if self.eat(b']') {
                return Ok(Json::Array(items));
            }
            if !self.eat(b',') {
                return Err("expected ',' or ']'");
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, &'static str> {
        let digits = self.bytes.get(self.at..self.at + 4).ok_or("short escape")?;
        if !digits.iter().all(u8::is_ascii_hexdigit) {
            return Err("bad escape");
        }
        self.at += 4;
        let digits = str::from_utf8(digits).map_err(|_| "bad escape")?;
        u32::from_str_radix(digits, 16).map_err(|_| "bad escape")
    }

    fn string(&mut self) -> Result<String, &'static str> {
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let byte = *self.bytes.get(self.at).ok_or("unterminated string")?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(out).map_err(|_| "invalid UTF-8"),
                0..0x20 => return Err("control character in string"),
                b'\\' => {
                    let escape = *self.bytes.get(self.at).ok_or("unterminated string")?;
                    self.at += 1;
                    let ch = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let first = self.hex4()?;
                            let code = match first {
                                0xD800..0xDC00 => {
                                    if !(self.eat(b'\\') && self.eat(b'u')) {
                                        return Err("lone surrogate");
                                    }
                                    let second = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&second) {
                                        return Err("lone surrogate");
                                    }
                                    0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                                }
                                0xDC00..0xE000 => return Err("lone surrogate"),
                                _ => first,
                            };
                            char::from_u32(code).ok_or("bad escape")?
                        }
                        _ => return Err("bad escape"),
                    };
                    out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
                _ => out.push(byte),
            }
        }
    }

    fn number(&mut self) -> Result<Json, &'static str> {
        let start = self.at;
        self.eat(b'-');
        match self.bytes.get(self.at) {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return Err("bad number"),
        }
        if self.eat(b'.') {
            if !self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
                return Err("bad number");
            }
            self.digits();
        }
        if self.eat(b'e') || self.eat(b'E') {
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            if !self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
                return Err("bad number");
            }
            self.digits();
        }
        str::from_utf8(&self.bytes[start..self.at])
            .ok()
            .and_then(|text| text.parse().ok())
            .map(Json::Number)
            .ok_or("bad number")
    }

    fn digits(&mut self) {
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
            self.at += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_values_and_escapes() {
        let json =
            parse(r#" {"a": [1, 2.5e1, -0, true, null], "b": "x\né😀\"", "c": {}} "#).unwrap();
        assert_eq!(json.get("a").unwrap().array().unwrap().len(), 5);
        assert_eq!(json.get("b").unwrap().str(), Some("x\né😀\""));
        assert_eq!(json.get("a").unwrap().array().unwrap()[1].u64(), Some(25));
        assert_eq!(json.get("c"), Some(&Json::Object(Vec::new())));
        assert_eq!(json.get("missing"), None);
    }

    #[test]
    fn rejects_malformed_input() {
        let deep = format!("{}1{}", "[".repeat(40), "]".repeat(40));
        for text in [
            "",
            "{",
            r#"{"a":1,}"#,
            r#"{"a":1} x"#,
            r#"{"a":1,"a":2}"#,
            r#"{"a" 1}"#,
            "[1,]",
            "01",
            "1.",
            "-",
            r#""abc"#,
            r#""\x""#,
            r#""\ud800""#,
            r#""\udc00""#,
            "\"tab\there\"",
            "tru",
            &deep,
        ] {
            assert!(parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn whole_numbers_only_for_u64() {
        assert_eq!(parse("2361857").unwrap().u64(), Some(2361857));
        for text in ["-1", "1.5", "1e300", r#""7""#] {
            assert_eq!(parse(text).unwrap().u64(), None, "{text}");
        }
    }
}
