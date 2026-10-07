//! A JSON reader for GitHub's release response. It rejects broken structure,
//! trailing data, control characters in strings and deep nesting. Strings stay
//! the raw text between their quotes. The fields Portside reads are plain ASCII
//! that must match exactly, so an escaped value can only fail to match.

#[derive(Debug, PartialEq)]
pub enum Json<'a> {
    /// A number, `true`, `false` or `null`, none of which Portside reads.
    Other,
    String(&'a str),
    Array(Vec<Json<'a>>),
    Object(Vec<(&'a str, Json<'a>)>),
}

const MAX_DEPTH: usize = 16;

pub fn parse(text: &str) -> Option<Json<'_>> {
    let mut reader = Reader { text, at: 0 };
    let value = reader.value(0)?;
    reader.space();
    (reader.at == text.len()).then_some(value)
}

impl<'a> Json<'a> {
    pub fn get(&self, key: &str) -> Option<&Json<'a>> {
        match self {
            Json::Object(fields) => fields.iter().find(|(k, _)| *k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&'a str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }
}

struct Reader<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        self.at += usize::from(found);
        found
    }

    fn value(&mut self, depth: usize) -> Option<Json<'a>> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.space();
        match self.peek()? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Json::String),
            // A number or literal, skipped without checking its spelling.
            b'-' | b'0'..=b'9' | b't' | b'f' | b'n' => {
                while matches!(
                    self.peek(),
                    Some(b'0'..=b'9' | b'a'..=b'z' | b'-' | b'+' | b'.' | b'E')
                ) {
                    self.at += 1;
                }
                Some(Json::Other)
            }
            _ => None,
        }
    }

    fn object(&mut self, depth: usize) -> Option<Json<'a>> {
        self.at += 1;
        let mut fields = Vec::new();
        self.space();
        if self.eat(b'}') {
            return Some(Json::Object(fields));
        }
        loop {
            self.space();
            let key = self.string()?;
            self.space();
            if !self.eat(b':') {
                return None;
            }
            fields.push((key, self.value(depth + 1)?));
            self.space();
            if self.eat(b'}') {
                return Some(Json::Object(fields));
            }
            if !self.eat(b',') {
                return None;
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Json<'a>> {
        self.at += 1;
        let mut items = Vec::new();
        self.space();
        if self.eat(b']') {
            return Some(Json::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.space();
            if self.eat(b']') {
                return Some(Json::Array(items));
            }
            if !self.eat(b',') {
                return None;
            }
        }
    }

    /// The text between the quotes, escapes and all.
    fn string(&mut self) -> Option<&'a str> {
        if !self.eat(b'"') {
            return None;
        }
        let text = self.text;
        let start = self.at;
        let mut end = start;
        loop {
            match *text.as_bytes().get(end)? {
                b'"' => break,
                0..0x20 => return None,
                b'\\' => end += 2,
                _ => end += 1,
            }
        }
        self.at = end + 1;
        Some(&text[start..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_values_and_keeps_escapes() {
        let json = parse(r#" {"a": [1, -2.5e1, true, null], "b": "x<\"é😀", "c": {}} "#).unwrap();
        assert!(matches!(json.get("a"), Some(Json::Array(items)) if items.len() == 4));
        assert_eq!(json.get("b").unwrap().str(), Some(r#"x<\"é😀"#));
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
            r#"{"a" 1}"#,
            "[1,]",
            r#""abc"#,
            r#""abc\""#,
            "\"tab\there\"",
            &deep,
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }
}
