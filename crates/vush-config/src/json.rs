//! A strict JSON reader (RFC 8259) that records a line and column for every
//! value and key. It never panics on any input and never copies text from the
//! file into an error message.

use crate::MAX_DEPTH;
use crate::error::{Errors, Location};
use crate::node::{Budget, EMPTY_FILE, Entry, Node, NodeKind, dedupe_keys, depth_message};
use crate::scalar::parse_number;

/// Reads `text` into a tree. `None` means a fatal problem was recorded.
pub(crate) fn read(text: &str, errors: &mut Errors) -> Option<Node> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        pos: 0,
        line: 1,
        column: 1,
        budget: Budget::new(),
        errors,
    };
    // A leading byte-order mark doesn't count toward the column.
    if reader.bytes.starts_with("\u{feff}".as_bytes()) {
        reader.pos = 3;
    }
    reader.skip_whitespace();
    if reader.peek().is_none() {
        reader.errors.push("", None, EMPTY_FILE);
        return None;
    }
    let node = reader.value(1).ok()?;
    reader.skip_whitespace();
    if reader.peek().is_some() {
        return reader.fatal("unexpected text after the JSON value").ok();
    }
    Some(node)
}

/// A fatal problem, already recorded.
struct Fatal;

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    column: u32,
    budget: Budget,
    errors: &'a mut Errors,
}

impl Reader<'_> {
    fn location(&self) -> Location {
        Location {
            line: self.line,
            column: self.column,
        }
    }

    fn fatal<T>(&mut self, message: &str) -> Result<T, Fatal> {
        let at = self.location();
        self.fatal_at(at, message)
    }

    fn fatal_at<T>(&mut self, at: Location, message: &str) -> Result<T, Fatal> {
        self.errors.push("", Some(at), message);
        Err(Fatal)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// Consumes one byte and moves the line and column. A continuation byte
    /// of a multi-byte character doesn't start a new column.
    fn advance(&mut self) {
        let Some(byte) = self.peek() else { return };
        let after_cr = self.pos > 0 && self.bytes.get(self.pos - 1) == Some(&b'\r');
        self.pos += 1;
        match byte {
            b'\r' => {
                self.line = self.line.saturating_add(1);
                self.column = 1;
            }
            // The second half of a CRLF pair is the same line break.
            b'\n' if after_cr => {}
            b'\n' => {
                self.line = self.line.saturating_add(1);
                self.column = 1;
            }
            b if b & 0xC0 == 0x80 => {}
            _ => self.column = self.column.saturating_add(1),
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.advance();
        }
    }

    fn charge(&mut self, at: Location, values: usize, text: usize) -> Result<(), Fatal> {
        match self.budget.charge(values, text) {
            Ok(()) => Ok(()),
            Err(message) => self.fatal_at(at, message),
        }
    }

    fn value(&mut self, level: usize) -> Result<Node, Fatal> {
        self.skip_whitespace();
        let at = self.location();
        let kind = match self.peek() {
            None => return self.fatal("the file ends before the value is complete"),
            Some(b'{') => self.map(level, at)?,
            Some(b'[') => self.list(level, at)?,
            Some(b'"') => {
                let text = self.string()?;
                self.charge(at, 1, text.len())?;
                NodeKind::String(text)
            }
            Some(b'-' | b'0'..=b'9') => self.number(at)?,
            Some(b't') => self.literal(b"true", NodeKind::Bool(true), at)?,
            Some(b'f') => self.literal(b"false", NodeKind::Bool(false), at)?,
            Some(b'n') => self.literal(b"null", NodeKind::Null, at)?,
            Some(_) => return self.fatal("unexpected character where a value should start"),
        };
        Ok(Node { location: at, kind })
    }

    fn literal(&mut self, word: &[u8], kind: NodeKind, at: Location) -> Result<NodeKind, Fatal> {
        if !self
            .bytes
            .get(self.pos..)
            .is_some_and(|rest| rest.starts_with(word))
        {
            return self.fatal("invalid literal, expected true, false or null");
        }
        for _ in word {
            self.advance();
        }
        self.charge(at, 1, 0)?;
        Ok(kind)
    }

    fn number(&mut self, at: Location) -> Result<NodeKind, Fatal> {
        let start = self.pos;
        while matches!(
            self.peek(),
            Some(b'-' | b'+' | b'.' | b'0'..=b'9' | b'e' | b'E')
        ) {
            self.advance();
        }
        let token = self
            .bytes
            .get(start..self.pos)
            .and_then(|b| std::str::from_utf8(b).ok());
        self.charge(at, 1, 0)?;
        match token.and_then(parse_number) {
            None => self.fatal_at(at, "invalid number"),
            Some(Ok(number)) => Ok(NodeKind::Number(number)),
            Some(Err(problem)) => {
                self.errors.push("", Some(at), problem.message());
                Ok(NodeKind::Invalid)
            }
        }
    }

    fn enter(&mut self, level: usize, at: Location) -> Result<(), Fatal> {
        if level > MAX_DEPTH {
            return self.fatal_at(at, &depth_message());
        }
        self.charge(at, 1, 0)
    }

    fn list(&mut self, level: usize, at: Location) -> Result<NodeKind, Fatal> {
        self.enter(level, at)?;
        self.advance();
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.advance();
            return Ok(NodeKind::List(items));
        }
        loop {
            items.push(self.value(level + 1)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.advance(),
                Some(b']') => {
                    self.advance();
                    return Ok(NodeKind::List(items));
                }
                _ => return self.fatal("expected a comma or the end of the list"),
            }
        }
    }

    fn map(&mut self, level: usize, at: Location) -> Result<NodeKind, Fatal> {
        self.enter(level, at)?;
        self.advance();
        let mut entries = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.advance();
            return Ok(NodeKind::Map(entries));
        }
        loop {
            self.skip_whitespace();
            let key_location = self.location();
            if self.peek() != Some(b'"') {
                return self.fatal("expected a quoted key");
            }
            let key = self.string()?;
            self.charge(key_location, 0, key.len())?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return self.fatal("expected a colon after the key");
            }
            self.advance();
            let value = self.value(level + 1)?;
            entries.push(Entry {
                key,
                key_location,
                value,
            });
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.advance(),
                Some(b'}') => {
                    self.advance();
                    let entries = dedupe_keys(entries, self.errors);
                    return Ok(NodeKind::Map(entries));
                }
                _ => return self.fatal("expected a comma or the end of the map"),
            }
        }
    }

    /// Reads a quoted string, starting at the opening quote.
    fn string(&mut self) -> Result<String, Fatal> {
        self.advance();
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(byte) = self.peek() else {
                return self.fatal("the string is never closed");
            };
            match byte {
                b'"' => {
                    self.advance();
                    return match String::from_utf8(out) {
                        Ok(text) => Ok(text),
                        Err(_) => self.fatal("the string isn't valid text"),
                    };
                }
                b'\\' => {
                    self.advance();
                    let c = self.escape()?;
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                0..=0x1F => return self.fatal("a control character must be escaped in a string"),
                _ => {
                    out.push(byte);
                    self.advance();
                }
            }
        }
    }

    /// Reads what follows a backslash.
    fn escape(&mut self) -> Result<char, Fatal> {
        let Some(byte) = self.peek() else {
            return self.fatal("the string is never closed");
        };
        let simple = match byte {
            b'"' => Some('"'),
            b'\\' => Some('\\'),
            b'/' => Some('/'),
            b'b' => Some('\u{8}'),
            b'f' => Some('\u{c}'),
            b'n' => Some('\n'),
            b'r' => Some('\r'),
            b't' => Some('\t'),
            _ => None,
        };
        if let Some(c) = simple {
            self.advance();
            return Ok(c);
        }
        if byte != b'u' {
            return self.fatal("invalid escape in a string");
        }
        self.advance();
        let first = self.hex4()?;
        let code = match first {
            0xD800..=0xDBFF => {
                if self.peek() != Some(b'\\') {
                    return self.fatal("a high surrogate needs a low surrogate after it");
                }
                self.advance();
                if self.peek() != Some(b'u') {
                    return self.fatal("a high surrogate needs a low surrogate after it");
                }
                self.advance();
                let second = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&second) {
                    return self.fatal("a high surrogate needs a low surrogate after it");
                }
                0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
            }
            0xDC00..=0xDFFF => return self.fatal("a low surrogate can't stand alone"),
            other => other,
        };
        match char::from_u32(code) {
            Some(c) => Ok(c),
            None => self.fatal("invalid unicode escape"),
        }
    }

    fn hex4(&mut self) -> Result<u32, Fatal> {
        let mut code = 0u32;
        for _ in 0..4 {
            let digit = self.peek().and_then(|b| char::from(b).to_digit(16));
            let Some(digit) = digit else {
                return self.fatal("a unicode escape needs four hex digits");
            };
            code = code * 16 + digit;
            self.advance();
        }
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConfigError, MAX_VALUES};
    use vush_core::plan::JsonNumber;

    const INVALID: &[&str] = &[
        "{\"a\":1,}",
        "[1,]",
        "{a:1}",
        "{'a':1}",
        "[01]",
        "[-01]",
        "[+1]",
        "[.5]",
        "[5.]",
        "[1e]",
        "[1e+]",
        "[NaN]",
        "[Infinity]",
        "[-Infinity]",
        "\"a\\qb\"",
        "\"a\\x41\"",
        "\"\\u12\"",
        "\"\\u12G4\"",
        "\"tab\there\"",
        "\"line\nbreak\"",
        "// c\n{}",
        "/* c */{}",
        "{} x",
        "{} {}",
        "[1 2]",
        "{\"a\" 1}",
        "{\"a\":}",
        "{,}",
        "[,]",
        "[",
        "{",
        "\"abc",
        "tru",
        "nul",
        "True",
        "\u{a0}{}",
        "\u{c}{}",
        "[\u{feff}1]",
        "\"\\ud83d\"",
        "\"\\ude00\"",
        "\"\\ud83dx\"",
        "\"\\ud83d\\u0041\"",
    ];

    const VALID: &[&str] = &[
        "null",
        "true",
        "false",
        "0",
        "-1",
        "2.5",
        "1e3",
        "1E+2",
        "1e-2",
        "1e-400",
        "\"\"",
        "\"x\"",
        "[]",
        "{}",
        "[[]]",
        "[{}]",
        "{\"a\":[1,{\"b\":null}]}",
        " \t\r\n[ 1 , 2 ]\n ",
        "\"\\u0000\"",
        "\"\\ud83d\\ude00\"",
        "\"é日本語😀\"",
        "\"\\/\"",
        "\"\u{7f}\"",
        "18446744073709551615",
        "-9223372036854775808",
        "{\"\":1}",
        "\"a\\\"b\\\\c\\/d\\be\\ff\\ng\\rh\\ti\"",
    ];

    fn read_all(text: &str) -> (Option<Node>, Vec<ConfigError>) {
        let mut errors = Errors::new();
        let node = read(text, &mut errors);
        (node, errors.into_sorted())
    }

    fn read_ok(text: &str) -> Node {
        let (node, errors) = read_all(text);
        assert!(errors.is_empty(), "{errors:?}");
        node.expect("a tree")
    }

    fn at(line: u32, column: u32) -> Location {
        Location { line, column }
    }

    fn same(node: &Node, value: &serde_json::Value) -> bool {
        use serde_json::Value;
        match (&node.kind, value) {
            (NodeKind::Null, Value::Null) => true,
            (NodeKind::Bool(a), Value::Bool(b)) => a == b,
            (NodeKind::String(a), Value::String(b)) => a == b,
            (NodeKind::Number(JsonNumber::Int(a)), Value::Number(b)) => b.as_i64() == Some(*a),
            (NodeKind::Number(JsonNumber::UInt(a)), Value::Number(b)) => b.as_u64() == Some(*a),
            (NodeKind::Number(JsonNumber::Float(a)), Value::Number(b)) => {
                b.is_f64()
                    && b.as_f64()
                        .is_some_and(|b| (a - b).abs() <= f64::EPSILON * a.abs())
            }
            (NodeKind::List(a), Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(n, v)| same(n, v))
            }
            (NodeKind::Map(a), Value::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|e| b.get(&e.key).is_some_and(|v| same(&e.value, v)))
            }
            _ => false,
        }
    }

    #[test]
    fn reads_every_value_kind() {
        let node = read_ok(
            r#"{"a": null, "b": [true, false], "c": 2.5, "d": "x", "e": {}, "f": [], "g": -3, "h": 18446744073709551615}"#,
        );
        assert_eq!(
            node.shape(),
            r#"{a:null,b:[true,false],c:2.5,d:"x",e:{},f:[],g:-3,h:18446744073709551615}"#
        );
    }

    #[test]
    fn strings_decode_every_escape() {
        let node = read_ok(r#""a\"b\\c\/d\be\ff\ng\rh\ti\u00e9\u0000""#);
        assert_eq!(
            node.kind,
            NodeKind::String("a\"b\\c/d\u{8}e\u{c}f\ng\rh\ti\u{e9}\u{0}".to_owned())
        );
    }

    #[test]
    fn surrogate_pairs_combine() {
        assert_eq!(
            read_ok(r#""\ud83d\ude00""#).kind,
            NodeKind::String("😀".to_owned())
        );
    }

    #[test]
    fn invalid_json_is_fatal_and_located() {
        for text in INVALID {
            let (node, errors) = read_all(text);
            assert!(node.is_none(), "{text:?}");
            assert_eq!(errors.len(), 1, "{text:?}");
            assert!(errors[0].location.is_some(), "{text:?}");
        }
    }

    #[test]
    fn locations_count_lines_and_characters() {
        let node = read_ok("{\n  \"é\": \"日本語\",\n  \"b\": [1,\n    2]\n}");
        assert_eq!(node.location, at(1, 1));
        let NodeKind::Map(entries) = &node.kind else {
            panic!("not a map")
        };
        assert_eq!(entries[0].key_location, at(2, 3));
        assert_eq!(entries[0].value.location, at(2, 8));
        assert_eq!(entries[1].key_location, at(3, 3));
        assert_eq!(entries[1].value.location, at(3, 8));
        let NodeKind::List(items) = &entries[1].value.kind else {
            panic!("not a list")
        };
        assert_eq!(items[0].location, at(3, 9));
        assert_eq!(items[1].location, at(4, 5));
    }

    #[test]
    fn line_endings_and_a_byte_order_mark_match_plain_files() {
        for text in ["{\r\n\"a\": 1\r\n}", "{\r\"a\": 1\r}", "{\n\"a\": 1\n}"] {
            let node = read_ok(text);
            let NodeKind::Map(entries) = &node.kind else {
                panic!("not a map")
            };
            assert_eq!(entries[0].key_location, at(2, 1), "{text:?}");
            assert_eq!(entries[0].value.location, at(2, 6), "{text:?}");
        }
        let node = read_ok("\u{feff}{\"a\": 1}");
        let NodeKind::Map(entries) = &node.kind else {
            panic!("not a map")
        };
        assert_eq!(node.location, at(1, 1));
        assert_eq!(entries[0].key_location, at(1, 2));
    }

    #[test]
    fn duplicates_are_recorded_and_the_first_kept() {
        let (node, errors) = read_all(r#"{"a": 1, "\u0061": 2, "b": 3}"#);
        assert_eq!(node.unwrap().shape(), "{a:1,b:3}");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].location, Some(at(1, 10)));
        assert!(
            errors[0]
                .message
                .contains("duplicate key `a`, first used on line 1")
        );
    }

    #[test]
    fn bad_numbers_are_recorded_and_reading_continues() {
        let (node, errors) = read_all("[18446744073709551616, 1e999, 2]");
        assert_eq!(node.unwrap().shape(), "[!,!,2]");
        assert_eq!(errors[0].location, Some(at(1, 2)));
        assert!(errors[0].message.contains("outside the supported range"));
        assert_eq!(errors[1].location, Some(at(1, 24)));
        assert!(errors[1].message.contains("too large"));
    }

    #[test]
    fn nesting_limit_is_128_levels() {
        read_ok(&format!("{}{}", "[".repeat(128), "]".repeat(128)));
        let (node, errors) = read_all(&format!("{}{}", "[".repeat(129), "]".repeat(129)));
        assert!(node.is_none());
        assert!(errors[0].message.contains("nesting") && errors[0].message.contains("128"));
        assert_eq!(errors[0].location, Some(at(1, 129)));

        let open = "{\"a\":";
        read_ok(&format!("{}1{}", open.repeat(128), "}".repeat(128)));
        let (node, errors) = read_all(&format!("{}1{}", open.repeat(129), "}".repeat(129)));
        assert!(node.is_none());
        assert!(errors[0].message.contains("nesting") && errors[0].message.contains("128"));
        assert_eq!(errors[0].location, Some(at(1, 5 * 128 + 1)));
    }

    #[test]
    fn value_limit_counts_every_value() {
        let list = |items: usize| format!("[{}0]", "0,".repeat(items - 1));
        read_ok(&list(MAX_VALUES - 1)); // The list itself is one value.
        let (node, errors) = read_all(&list(MAX_VALUES));
        assert!(node.is_none());
        assert!(errors[0].message.contains("250000 values"));
    }

    #[test]
    fn empty_files_say_so() {
        for text in ["", "  \n\t\r\n", "\u{feff}"] {
            let (node, errors) = read_all(text);
            assert!(node.is_none(), "{text:?}");
            assert!(errors[0].message.contains("the file is empty"), "{text:?}");
            assert_eq!(errors[0].location, None, "{text:?}");
        }
    }

    #[test]
    fn minified_files_track_columns_cheaply() {
        let items = 200_000;
        let node = read_ok(&format!("[{}7]", "0,".repeat(items - 1)));
        let NodeKind::List(list) = &node.kind else {
            panic!("not a list")
        };
        assert_eq!(list[items - 1].location, at(1, 400_000));
    }

    #[test]
    fn agrees_with_serde_json() {
        for text in VALID.iter().chain(INVALID) {
            let (node, errors) = read_all(text);
            let ours = node.filter(|_| errors.is_empty());
            let theirs = serde_json::from_str::<serde_json::Value>(text).ok();
            match (ours, theirs) {
                (None, None) => {}
                (Some(node), Some(value)) => assert!(same(&node, &value), "{text:?}"),
                (ours, theirs) => panic!("{text:?}: ours {ours:?}, theirs {theirs:?}"),
            }
        }
    }

    #[test]
    fn errors_never_contain_file_text() {
        let (_, errors) = read_all(r#"{"SECRETKEY": SECRETVALUE}"#);
        assert!(!errors[0].message.contains("SECRET"));
    }
}
