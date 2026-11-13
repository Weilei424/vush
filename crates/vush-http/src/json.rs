//! Compact JSON encoding of a resolved [`JsonValue`].

use vush_core::plan::JsonNumber;
use vush_core::protocol::JsonValue;

/// Appends `value` to `out` as compact JSON.
///
/// Members keep their order. Strings escape `"`, `\` and every character
/// below U+0020, and write everything else as UTF-8. `Int` and `UInt` are
/// written exactly, and `Float` uses Rust's shortest round-trip `Display`.
pub(crate) fn encode(value: &JsonValue, out: &mut Vec<u8>) {
    match value {
        JsonValue::Null => out.extend_from_slice(b"null"),
        JsonValue::Bool(true) => out.extend_from_slice(b"true"),
        JsonValue::Bool(false) => out.extend_from_slice(b"false"),
        JsonValue::Number(JsonNumber::Int(n)) => out.extend_from_slice(n.to_string().as_bytes()),
        JsonValue::Number(JsonNumber::UInt(n)) => out.extend_from_slice(n.to_string().as_bytes()),
        JsonValue::Number(JsonNumber::Float(n)) => out.extend_from_slice(n.to_string().as_bytes()),
        JsonValue::String(text) => string(text, out),
        JsonValue::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                encode(item, out);
            }
            out.push(b']');
        }
        JsonValue::Object(members) => {
            out.push(b'{');
            for (i, (key, item)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                string(key, out);
                out.push(b':');
                encode(item, out);
            }
            out.push(b'}');
        }
    }
}

fn string(text: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in text.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if c < ' ' => out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes()),
            c => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_compact_with_exact_numbers_and_escapes() {
        let s = |t: &str| JsonValue::String(t.to_owned());
        let value = JsonValue::Object(vec![
            ("s".into(), s("\"\\/\u{8}\u{c}\n\r\t\u{1}\u{1f}é")),
            ("i".into(), JsonValue::Number(JsonNumber::Int(i64::MIN))),
            ("u".into(), JsonValue::Number(JsonNumber::UInt(u64::MAX))),
            ("f".into(), JsonValue::Number(JsonNumber::Float(1.5))),
            ("z".into(), JsonValue::Number(JsonNumber::Float(-0.0))),
            (
                "a".into(),
                JsonValue::Array(vec![
                    JsonValue::Null,
                    JsonValue::Bool(true),
                    JsonValue::Object(vec![]),
                ]),
            ),
        ]);
        let mut out = Vec::new();
        encode(&value, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"s":"\"\\/\b\f\n\r\t\u0001\u001fé","i":-9223372036854775808,"u":18446744073709551615,"f":1.5,"z":-0,"a":[null,true,{}]}"#
        );
    }
}
