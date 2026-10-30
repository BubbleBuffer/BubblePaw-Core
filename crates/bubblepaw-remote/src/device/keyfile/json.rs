use super::*;

// Strict JSON for the key-file domain: objects, strings, integers and null only.
// Duplicate keys, floats, booleans, arrays, lone surrogates and control characters are refused.
// Canonical output follows RFC 8785 (JCS): UTF-16 code-unit key order and minimal escapes.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Json {
    Null,
    Int(i64),
    Str(Zeroizing<String>),
    Object(Vec<(String, Json)>),
}

pub(super) fn jstr(value: &str) -> Json {
    Json::Str(Zeroizing::new(value.to_owned()))
}

pub(super) fn object<const N: usize>(entries: [(&str, Json); N]) -> Json {
    Json::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

pub(super) fn parse_document(text: &str) -> Result<Json, DeviceKeyError> {
    let mut reader = Reader { text, pos: 0 };
    reader.skip_ws();
    let value = reader.value(0)?;
    reader.skip_ws();
    if reader.pos != text.len() {
        return Err(DeviceKeyError::Malformed(
            "trailing data after the document",
        ));
    }
    Ok(value)
}

struct Reader<'a> {
    text: &'a str,
    pos: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// `depth` is the nesting level of the enclosing object (0 at the top level).
    fn value(&mut self, depth: usize) -> Result<Json, DeviceKeyError> {
        match self.peek() {
            Some(b'{') => self.object(depth + 1),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'-' | b'0'..=b'9') => self.integer(),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b't' | b'f') => Err(DeviceKeyError::Malformed(
                "boolean values are not supported",
            )),
            Some(b'[') => Err(DeviceKeyError::Malformed("arrays are not supported")),
            _ => Err(DeviceKeyError::Malformed("unexpected token")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, DeviceKeyError> {
        if depth > MAX_JSON_DEPTH {
            return Err(DeviceKeyError::Malformed("objects are nested too deeply"));
        }
        self.pos += 1;
        let mut entries: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.eat(b'}') {
            return Ok(Json::Object(entries));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(DeviceKeyError::Malformed("expected an object key"));
            }
            let key = self.string()?.to_string();
            self.skip_ws();
            if !self.eat(b':') {
                return Err(DeviceKeyError::Malformed("expected ':' after a key"));
            }
            self.skip_ws();
            let value = self.value(depth)?;
            if entries.iter().any(|(existing, _)| *existing == key) {
                return Err(DeviceKeyError::Malformed("duplicate key"));
            }
            entries.push((key, value));
            self.skip_ws();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b'}') {
                return Ok(Json::Object(entries));
            }
            return Err(DeviceKeyError::Malformed("expected ',' or '}'"));
        }
    }

    fn string(&mut self) -> Result<Zeroizing<String>, DeviceKeyError> {
        self.pos += 1;
        let mut out = Zeroizing::new(String::new());
        loop {
            let ch = self
                .rest()
                .chars()
                .next()
                .ok_or(DeviceKeyError::Malformed("unterminated string"))?;
            self.pos += ch.len_utf8();
            match ch {
                '"' => return Ok(out),
                '\\' => out.push(self.escape()?),
                control if (control as u32) < 0x20 => {
                    return Err(DeviceKeyError::Malformed("control character in string"));
                }
                plain => out.push(plain),
            }
        }
    }

    fn escape(&mut self) -> Result<char, DeviceKeyError> {
        let ch = self
            .rest()
            .chars()
            .next()
            .ok_or(DeviceKeyError::Malformed("unterminated escape"))?;
        self.pos += ch.len_utf8();
        Ok(match ch {
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => {
                let high = self.hex4()?;
                if (0xD800..=0xDBFF).contains(&high) {
                    if !self.rest().starts_with("\\u") {
                        return Err(DeviceKeyError::Malformed("lone surrogate in string"));
                    }
                    self.pos += 2;
                    let low = self.hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&low) {
                        return Err(DeviceKeyError::Malformed("lone surrogate in string"));
                    }
                    char::from_u32(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00))
                        .ok_or(DeviceKeyError::Malformed("invalid escape"))?
                } else {
                    char::from_u32(high)
                        .ok_or(DeviceKeyError::Malformed("lone surrogate in string"))?
                }
            }
            _ => return Err(DeviceKeyError::Malformed("invalid escape")),
        })
    }

    fn hex4(&mut self) -> Result<u32, DeviceKeyError> {
        let digits = self
            .text
            .get(self.pos..self.pos + 4)
            .ok_or(DeviceKeyError::Malformed("invalid unicode escape"))?;
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DeviceKeyError::Malformed("invalid unicode escape"));
        }
        self.pos += 4;
        u32::from_str_radix(digits, 16)
            .map_err(|_| DeviceKeyError::Malformed("invalid unicode escape"))
    }

    fn integer(&mut self) -> Result<Json, DeviceKeyError> {
        let start = self.pos;
        let negative = self.eat(b'-');
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(DeviceKeyError::Malformed("leading zero in number"));
                }
            }
            Some(b'1'..=b'9') => self.skip_digits(),
            _ => return Err(DeviceKeyError::Malformed("invalid number")),
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err(DeviceKeyError::Malformed(
                "fractions and exponents are not supported",
            ));
        }
        let digits = &self.text[start..self.pos];
        if negative && digits == "-0" {
            return Err(DeviceKeyError::Malformed("negative zero is not supported"));
        }
        digits
            .parse::<i64>()
            .map(Json::Int)
            .map_err(|_| DeviceKeyError::Malformed("integer out of range"))
    }

    fn skip_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, DeviceKeyError> {
        if self.rest().starts_with(word) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(DeviceKeyError::Malformed("unexpected token"))
        }
    }
}

/// RFC 8785 canonical form: keys sorted by UTF-16 code units, minimal string escapes.
pub(super) fn canonical(value: &Json) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

pub(super) fn write_canonical(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Int(number) => out.push_str(&number.to_string()),
        Json::Str(text) => write_string(text, out),
        Json::Object(entries) => {
            let mut sorted: Vec<&(String, Json)> = entries.iter().collect();
            sorted.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            out.push('{');
            for (index, (key, child)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_canonical(child, out);
            }
            out.push('}');
        }
    }
}

pub(super) fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32))
            }
            plain => out.push(plain),
        }
    }
    out.push('"');
}

/// The stored file layout: fields in format order, two-space indentation.
pub(super) fn write_pretty(value: &Json, depth: usize, out: &mut String) {
    match value {
        Json::Object(entries) if !entries.is_empty() => {
            out.push_str("{\n");
            for (index, (key, child)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push_str(",\n");
                }
                for _ in 0..=depth {
                    out.push_str("  ");
                }
                write_string(key, out);
                out.push_str(": ");
                write_pretty(child, depth + 1, out);
            }
            out.push('\n');
            for _ in 0..depth {
                out.push_str("  ");
            }
            out.push('}');
        }
        other => write_canonical(other, out),
    }
}
