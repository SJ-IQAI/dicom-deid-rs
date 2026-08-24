//! A minimal JSON reader, just large enough for mapper files (r-7-4).
//!
//! The crate deliberately carries no serialization framework, so rather
//! than pull one in for a file that holds nothing but string pairs, this
//! parses RFC 8259 JSON directly. It is a strict parser: trailing
//! commas, comments, unquoted keys, and lone surrogates are all errors,
//! so a malformed mapper file is reported rather than silently reduced
//! to a partial map.

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    /// The number's source text, kept verbatim. Mapper keys and values
    /// are identifiers, so round-tripping through `f64` would be a way
    /// to lose digits, not to gain anything.
    Number(String),
    String(String),
    Array(Vec<JsonValue>),
    /// Members in document order.
    ///
    /// A `Vec` rather than a map so that duplicate keys survive parsing:
    /// two entries for one PatientID must be reported (r-7-5), and a map
    /// would silently keep only the last.
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    /// The name of this value's type, for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            JsonValue::Null => "null",
            JsonValue::Bool(_) => "a boolean",
            JsonValue::Number(_) => "a number",
            JsonValue::String(_) => "a string",
            JsonValue::Array(_) => "an array",
            JsonValue::Object(_) => "an object",
        }
    }
}

/// Guards against stack exhaustion on deeply nested input. Mapper files
/// are two levels deep at most, so this is far above any legitimate use.
const MAX_DEPTH: usize = 64;

/// Parse a complete JSON document.
pub fn parse(text: &str) -> Result<JsonValue, String> {
    // A UTF-8 BOM is not part of the JSON grammar, but editors and
    // Windows tooling emit one often enough that rejecting it would be
    // unhelpful.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
        depth: 0,
    };
    parser.skip_whitespace();
    let value = parser.parse_value()?;
    parser.skip_whitespace();
    if parser.pos < parser.bytes.len() {
        return Err(parser.error("unexpected trailing content"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected '{}'", byte as char)))
        }
    }

    /// Report an error at the current position, located by line and
    /// column so a large mapper file can be corrected by hand.
    fn error(&self, message: &str) -> String {
        let consumed = &self.bytes[..self.pos.min(self.bytes.len())];
        let line = 1 + consumed.iter().filter(|b| **b == b'\n').count();
        let column = match consumed.iter().rposition(|b| *b == b'\n') {
            Some(index) => self.pos - index,
            None => self.pos + 1,
        };
        format!("{} at line {}, column {}", message, line, column)
    }

    fn parse_value(&mut self) -> Result<JsonValue, String> {
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') => self.parse_literal("true").map(|_| JsonValue::Bool(true)),
            Some(b'f') => self.parse_literal("false").map(|_| JsonValue::Bool(false)),
            Some(b'n') => self.parse_literal("null").map(|_| JsonValue::Null),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("nesting is too deep"));
        }
        Ok(())
    }

    fn parse_object(&mut self) -> Result<JsonValue, String> {
        self.enter()?;
        self.expect(b'{')?;
        let mut members: Vec<(String, JsonValue)> = Vec::new();

        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonValue::Object(members));
        }

        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a quoted member name"));
            }
            let name = self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            let value = self.parse_value()?;
            members.push((name, value));

            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }

        self.depth -= 1;
        Ok(JsonValue::Object(members))
    }

    fn parse_array(&mut self) -> Result<JsonValue, String> {
        self.enter()?;
        self.expect(b'[')?;
        let mut items = Vec::new();

        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonValue::Array(items));
        }

        loop {
            self.skip_whitespace();
            items.push(self.parse_value()?);

            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }

        self.depth -= 1;
        Ok(JsonValue::Array(items))
    }

    fn parse_literal(&mut self, word: &str) -> Result<(), String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(())
        } else {
            Err(self.error("unexpected character"))
        }
    }

    fn parse_number(&mut self) -> Result<JsonValue, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            // A leading zero may not be followed by more digits.
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.skip_digits(),
            _ => return Err(self.error("expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected a digit after '.'"));
            }
            self.skip_digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected a digit in the exponent"));
            }
            self.skip_digits();
        }

        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.error("invalid number"))?;
        Ok(JsonValue::Number(text.to_string()))
    }

    fn skip_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        // Bytes are copied through verbatim, so multi-byte UTF-8 in the
        // source survives without being decoded and re-encoded.
        let mut out: Vec<u8> = Vec::new();
        loop {
            let byte = match self.peek() {
                Some(byte) => byte,
                None => return Err(self.error("unterminated string")),
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => {
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                }
                // RFC 8259: control characters must be escaped.
                0x00..=0x1F => return Err(self.error("unescaped control character in string")),
                _ => {
                    out.push(byte);
                    self.pos += 1;
                }
            }
        }
        String::from_utf8(out).map_err(|_| self.error("invalid UTF-8 in string"))
    }

    fn parse_escape(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        let escape = match self.peek() {
            Some(byte) => byte,
            None => return Err(self.error("unterminated escape sequence")),
        };
        self.pos += 1;
        let literal = match escape {
            b'"' => b'"',
            b'\\' => b'\\',
            b'/' => b'/',
            b'b' => 0x08,
            b'f' => 0x0C,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'u' => {
                let ch = self.parse_unicode_escape()?;
                let mut buffer = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                return Ok(());
            }
            _ => return Err(self.error("unknown escape sequence")),
        };
        out.push(literal);
        Ok(())
    }

    /// Parse `\uXXXX`, joining a surrogate pair into one code point.
    fn parse_unicode_escape(&mut self) -> Result<char, String> {
        let first = self.parse_hex4()?;
        // Not a surrogate: a complete code point on its own.
        if !(0xD800..=0xDFFF).contains(&first) {
            return char::from_u32(first as u32)
                .ok_or_else(|| self.error("invalid \\u escape sequence"));
        }
        if !(0xD800..=0xDBFF).contains(&first) {
            return Err(self.error("unpaired low surrogate in \\u escape sequence"));
        }
        if self.peek() != Some(b'\\') {
            return Err(self.error("high surrogate is not followed by a low surrogate"));
        }
        self.pos += 1;
        if self.peek() != Some(b'u') {
            return Err(self.error("high surrogate is not followed by a low surrogate"));
        }
        self.pos += 1;
        let second = self.parse_hex4()?;
        if !(0xDC00..=0xDFFF).contains(&second) {
            return Err(self.error("high surrogate is not followed by a low surrogate"));
        }
        let combined = 0x10000 + (((first as u32) - 0xD800) << 10) + ((second as u32) - 0xDC00);
        char::from_u32(combined).ok_or_else(|| self.error("invalid surrogate pair"))
    }

    fn parse_hex4(&mut self) -> Result<u16, String> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let digit = match self.peek() {
                Some(byte @ b'0'..=b'9') => byte - b'0',
                Some(byte @ b'a'..=b'f') => byte - b'a' + 10,
                Some(byte @ b'A'..=b'F') => byte - b'A' + 10,
                _ => return Err(self.error("expected four hex digits after \\u")),
            };
            value = (value << 4) | digit as u16;
            self.pos += 1;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(text: &str) -> Vec<(String, JsonValue)> {
        match parse(text).expect("should parse") {
            JsonValue::Object(members) => members,
            other => panic!("expected an object, got {}", other.type_name()),
        }
    }

    #[test]
    fn parses_a_flat_string_object() {
        assert_eq!(
            object(r#"{"a": "1", "b": "2"}"#),
            vec![
                ("a".to_string(), JsonValue::String("1".into())),
                ("b".to_string(), JsonValue::String("2".into())),
            ]
        );
    }

    #[test]
    fn preserves_duplicate_member_names() {
        // A map would drop one of these; reporting the duplicate is the
        // whole reason members are kept as a Vec (r-7-5).
        assert_eq!(object(r#"{"a": "1", "a": "2"}"#).len(), 2);
    }

    #[test]
    fn parses_nested_arrays_and_objects() {
        let value = parse(r#"[["a", "b"], {"k": "v"}, [], {}]"#).expect("should parse");
        let items = match value {
            JsonValue::Array(items) => items,
            other => panic!("expected an array, got {}", other.type_name()),
        };
        assert_eq!(items.len(), 4);
        assert_eq!(
            items[0],
            JsonValue::Array(vec![
                JsonValue::String("a".into()),
                JsonValue::String("b".into())
            ])
        );
        assert_eq!(
            items[1],
            JsonValue::Object(vec![("k".to_string(), JsonValue::String("v".into()))])
        );
        assert_eq!(items[2], JsonValue::Array(vec![]));
        assert_eq!(items[3], JsonValue::Object(vec![]));
    }

    #[test]
    fn parses_scalars() {
        assert_eq!(parse("null").expect("parses"), JsonValue::Null);
        assert_eq!(parse("true").expect("parses"), JsonValue::Bool(true));
        assert_eq!(parse("false").expect("parses"), JsonValue::Bool(false));
        assert_eq!(
            parse("-12.5e3").expect("parses"),
            JsonValue::Number("-12.5e3".into())
        );
        assert_eq!(parse("0").expect("parses"), JsonValue::Number("0".into()));
    }

    #[test]
    fn keeps_number_text_verbatim() {
        // A long numeric MRN must not be rounded through f64.
        assert_eq!(
            parse("900719925474099123").expect("parses"),
            JsonValue::Number("900719925474099123".into())
        );
    }

    #[test]
    fn parses_escapes_and_unicode() {
        assert_eq!(
            parse(r#""a\"b\\c\/d\b\f\n\r\te\u0041""#).expect("parses"),
            JsonValue::String("a\"b\\c/d\u{08}\u{0C}\n\r\teA".into())
        );
    }

    #[test]
    fn parses_surrogate_pairs() {
        assert_eq!(
            parse(r#""\uD83D\uDE00""#).expect("parses"),
            JsonValue::String("\u{1F600}".into())
        );
    }

    #[test]
    fn preserves_multibyte_utf8() {
        assert_eq!(
            parse("\"Müller☂\"").expect("parses"),
            JsonValue::String("Müller☂".into())
        );
    }

    #[test]
    fn strips_a_byte_order_mark() {
        assert_eq!(object("\u{feff}{\"a\": \"1\"}").len(), 1);
    }

    #[test]
    fn rejects_malformed_documents() {
        for bad in [
            "",
            "{",
            "{\"a\": }",
            "{\"a\": \"1\",}", // trailing comma
            "[1, 2,]",         // trailing comma
            "{a: 1}",          // unquoted key
            "{\"a\" \"1\"}",   // missing colon
            "\"unterminated",
            "01", // leading zero
            "-",
            "1.",
            "1e",
            "{} {}",           // trailing content
            "\"\\q\"",         // unknown escape
            "\"\\u00\"",       // short escape
            "\"\\uD83D\"",     // unpaired high surrogate
            "\"\\uDE00\"",     // unpaired low surrogate
            "\"line\nbreak\"", // unescaped control character
            "// comment\n{}",
        ] {
            assert!(parse(bad).is_err(), "should reject {:?} but it parsed", bad);
        }
    }

    #[test]
    fn rejects_input_nested_past_the_depth_limit() {
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(parse(&deep).is_err(), "should reject over-deep nesting");
    }

    #[test]
    fn errors_report_a_line_and_column() {
        let err = parse("{\n  \"a\": \"1\",\n  \"b\":\n}").expect_err("should fail");
        assert!(err.contains("line 4"), "unexpected error: {}", err);
    }
}
