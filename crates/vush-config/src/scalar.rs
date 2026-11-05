//! The number rule and the typing rule for unquoted YAML values. Both readers
//! call `parse_number`, so a number reads the same in YAML and JSON.

use vush_core::plan::JsonNumber;

/// Message for an unquoted value that YAML versions read differently.
pub(crate) const AMBIGUOUS_NUMBER: &str = "unquoted value looks like a number, but YAML versions read it differently. Quote it, or write it as a plain decimal";

/// Why a well-formed number can't be kept exactly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NumberProblem {
    OutOfRange,
    TooLarge,
}

impl NumberProblem {
    pub(crate) fn message(self) -> &'static str {
        match self {
            NumberProblem::OutOfRange => "integer outside the supported range",
            NumberProblem::TooLarge => "number too large",
        }
    }
}

/// `None` when `text` isn't in JSON's number grammar.
pub(crate) fn parse_number(text: &str) -> Option<Result<JsonNumber, NumberProblem>> {
    let bytes = text.as_bytes();
    let mut i = usize::from(bytes.first() == Some(&b'-'));
    match bytes.get(i)? {
        b'0' => i += 1,
        b'1'..=b'9' => i += digits(&bytes[i..]),
        _ => return None,
    }
    let mut is_float = false;
    if bytes.get(i) == Some(&b'.') {
        let n = digits(&bytes[i + 1..]);
        if n == 0 {
            return None;
        }
        i += 1 + n;
        is_float = true;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(bytes.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        let n = digits(&bytes[j..]);
        if n == 0 {
            return None;
        }
        i = j + n;
        is_float = true;
    }
    if i != bytes.len() {
        return None;
    }
    if is_float {
        return Some(match text.parse::<f64>() {
            Ok(value) if value.is_finite() => Ok(JsonNumber::Float(value)),
            _ => Err(NumberProblem::TooLarge),
        });
    }
    if let Ok(value) = text.parse::<i64>() {
        return Some(Ok(JsonNumber::Int(value)));
    }
    Some(match text.parse::<u64>() {
        Ok(value) => Ok(JsonNumber::UInt(value)),
        Err(_) => Err(NumberProblem::OutOfRange),
    })
}

/// Length of the run of ASCII digits at the start of `bytes`.
fn digits(bytes: &[u8]) -> usize {
    bytes.iter().take_while(|b| b.is_ascii_digit()).count()
}

/// How an unquoted YAML value reads.
#[derive(Debug, PartialEq)]
pub(crate) enum Plain {
    Null,
    Bool(bool),
    Number(JsonNumber),
    BadNumber(NumberProblem),
    Ambiguous,
    String,
}

pub(crate) fn classify_plain(text: &str) -> Plain {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Plain::Null,
        "true" | "True" | "TRUE" => return Plain::Bool(true),
        "false" | "False" | "FALSE" => return Plain::Bool(false),
        _ => {}
    }
    match parse_number(text) {
        Some(Ok(number)) => Plain::Number(number),
        Some(Err(problem)) => Plain::BadNumber(problem),
        None if is_ambiguous(text) => Plain::Ambiguous,
        None => Plain::String,
    }
}

/// Number forms that some YAML versions read as numbers. Only called on text
/// that isn't in JSON's number grammar.
fn is_ambiguous(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let body = lower.strip_prefix(['+', '-']).unwrap_or(&lower).as_bytes();
    if body == b".inf" || body == b".nan" {
        return true;
    }
    if let [b'0', base, rest @ ..] = body {
        let valid = |b: &u8| {
            *b == b'_'
                || match base {
                    b'x' => b.is_ascii_hexdigit(),
                    b'o' => (b'0'..=b'7').contains(b),
                    b'b' => matches!(b, b'0' | b'1'),
                    _ => false,
                }
        };
        if matches!(base, b'x' | b'o' | b'b') && !rest.is_empty() && rest.iter().all(valid) {
            return true;
        }
    }
    if !matches!(body.first(), Some(b'0'..=b'9' | b'.')) {
        return false;
    }
    let clean: Vec<u8> = body.iter().copied().filter(|b| *b != b'_').collect();
    let int = digits(&clean);
    let mut i = int;
    let mut frac = 0;
    if clean.get(i) == Some(&b'.') {
        frac = digits(&clean[i + 1..]);
        i += 1 + frac;
    } else if int == 0 {
        return false;
    }
    if int == 0 && frac == 0 {
        return false;
    }
    if matches!(clean.get(i), Some(b'e')) {
        let mut j = i + 1;
        if matches!(clean.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        let n = digits(&clean[j..]);
        if n == 0 {
            return false;
        }
        i = j + n;
    }
    i == clean.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use JsonNumber::{Float, Int, UInt};

    #[test]
    fn numbers_keep_their_exact_variant() {
        for (text, want) in [
            ("0", Int(0)),
            ("-0", Int(0)),
            ("2", Int(2)),
            ("-3", Int(-3)),
            ("9223372036854775807", Int(i64::MAX)),
            ("-9223372036854775808", Int(i64::MIN)),
            ("9223372036854775808", UInt(1 << 63)),
            ("18446744073709551615", UInt(u64::MAX)),
            ("2.5", Float(2.5)),
            ("-2.5", Float(-2.5)),
            ("0.15", Float(0.15)),
            ("1e3", Float(1000.0)),
            ("1E+2", Float(100.0)),
            ("1e-2", Float(0.01)),
            ("1e-400", Float(0.0)),
        ] {
            assert_eq!(parse_number(text), Some(Ok(want)), "{text}");
        }
        // -0.0 compares equal to 0.0, so check the sign separately.
        let Some(Ok(Float(value))) = parse_number("-0.0") else {
            panic!("not a float")
        };
        assert!(value == 0.0 && value.is_sign_negative());
    }

    #[test]
    fn numbers_out_of_range_are_problems_never_floats() {
        for text in [
            "18446744073709551616",
            "-9223372036854775809",
            "123456789012345678901234567890",
        ] {
            assert_eq!(
                parse_number(text),
                Some(Err(NumberProblem::OutOfRange)),
                "{text}"
            );
        }
        for text in ["1e999", "-1e999", "1.5e400"] {
            assert_eq!(
                parse_number(text),
                Some(Err(NumberProblem::TooLarge)),
                "{text}"
            );
        }
        assert!(
            NumberProblem::OutOfRange
                .message()
                .contains("outside the supported range")
        );
        assert!(NumberProblem::TooLarge.message().contains("too large"));
    }

    #[test]
    fn text_outside_the_json_grammar_is_not_a_number() {
        for text in [
            "", "-", "+5", "010", "-010", "00", ".5", "5.", "1e", "1e+", "1.e3", "0x10", "1_000",
            ".inf", "NaN", "Infinity", "1 ", " 1", "1.2.3", "٣", "５",
        ] {
            assert_eq!(parse_number(text), None, "{text:?}");
        }
    }

    #[test]
    fn plain_values_follow_the_table() {
        for text in ["", "~", "null", "Null", "NULL"] {
            assert_eq!(classify_plain(text), Plain::Null, "{text:?}");
        }
        for text in ["true", "True", "TRUE"] {
            assert_eq!(classify_plain(text), Plain::Bool(true), "{text}");
        }
        for text in ["false", "False", "FALSE"] {
            assert_eq!(classify_plain(text), Plain::Bool(false), "{text}");
        }
        for (text, want) in [
            ("2", Int(2)),
            ("-3", Int(-3)),
            ("2.5", Float(2.5)),
            ("1e3", Float(1000.0)),
        ] {
            assert_eq!(classify_plain(text), Plain::Number(want), "{text}");
        }
        assert_eq!(
            classify_plain("18446744073709551616"),
            Plain::BadNumber(NumberProblem::OutOfRange)
        );
        assert_eq!(
            classify_plain("1e999"),
            Plain::BadNumber(NumberProblem::TooLarge)
        );
    }

    #[test]
    fn ambiguous_number_forms_are_flagged() {
        for text in [
            "010", "-010", "+5", "-012", "089", ".5", "5.", "1.e3", ".5e3", "+1.5", "1_000", "1_",
            "0x10", "0X10", "-0x10", "0o7", "0b1", "0x_", "0b1_0", ".inf", "-.inf", "+.INF",
            ".Inf", ".nan", ".NaN", "-.NaN",
        ] {
            assert_eq!(classify_plain(text), Plain::Ambiguous, "{text}");
        }
    }

    #[test]
    fn everything_else_is_a_string() {
        for text in [
            "yes",
            "no",
            "on",
            "off",
            "y",
            "n",
            "NO",
            "Yes",
            "nULL",
            "tRUE",
            "5m",
            "2001-01-01",
            "1:30",
            "1.2.3",
            "_1",
            "0x",
            "0xZZ",
            "0b2",
            "0o8",
            "-",
            ".",
            "e5",
            "1e",
            "20/s",
            "1%",
            "127.0.0.1",
            "500ms",
            "1h30m",
            "1MiB",
            "0/s",
            "abc",
            "-abc",
            "+abc",
            "${token}",
        ] {
            assert_eq!(classify_plain(text), Plain::String, "{text}");
        }
    }
}
