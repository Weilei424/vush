//! JSON paths: a small `$.a.b[0]` subset, split into segments up front.

use std::str::FromStr;

use super::value::ValueError;

const EXPECTED: &str =
    "a JSON path such as $, $.name or $.items[0].id (names use A-Z, a-z, 0-9, _ and -)";

/// Whether `c` may appear in a `.name` segment: `[A-Za-z0-9_-]`.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// One step of a [`JsonPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonPathSegment {
    /// `.name`: an object field.
    Field(String),
    /// `[N]`: an array element.
    Index(u32),
}

/// A path into a JSON document.
///
/// `$` alone is the whole document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPath {
    segments: Vec<JsonPathSegment>,
}

impl JsonPath {
    /// The segments in order. Empty for `$`.
    pub fn segments(&self) -> &[JsonPathSegment] {
        &self.segments
    }
}

impl FromStr for JsonPath {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let segments = || {
            let mut rest = text.strip_prefix('$')?;
            let mut segments = Vec::new();
            while !rest.is_empty() {
                if let Some(after) = rest.strip_prefix('.') {
                    let end = after.find(|c| !is_name_char(c)).unwrap_or(after.len());
                    if end == 0 {
                        return None;
                    }
                    segments.push(JsonPathSegment::Field(after[..end].to_owned()));
                    rest = &after[end..];
                } else {
                    // Anything that is not `.name` must be `[N]`.
                    let (digits, after) = rest.strip_prefix('[')?.split_once(']')?;
                    // `u32::from_str` also accepts a leading `+`, so check the digits first.
                    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                        return None;
                    }
                    segments.push(JsonPathSegment::Index(digits.parse().ok()?));
                    rest = after;
                }
            }
            Some(segments)
        };
        segments()
            .map(|segments| JsonPath { segments })
            .ok_or_else(|| ValueError {
                expected: EXPECTED,
                input: text.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::JsonPathSegment::{Field, Index};
    use super::*;

    fn field(name: &str) -> JsonPathSegment {
        Field(name.to_string())
    }

    #[test]
    fn paths_parse_to_segments() {
        let cases: Vec<(&str, Vec<JsonPathSegment>)> = vec![
            ("$", vec![]),
            ("$.a", vec![field("a")]),
            ("$.access_token", vec![field("access_token")]),
            ("$.a-b", vec![field("a-b")]),
            ("$.accessToken", vec![field("accessToken")]),
            ("$.0", vec![field("0")]),
            ("$[2]", vec![Index(2)]),
            ("$[4294967295]", vec![Index(u32::MAX)]),
            ("$[01]", vec![Index(1)]),
            (
                "$.a.b[0].c",
                vec![field("a"), field("b"), Index(0), field("c")],
            ),
            ("$.items[0][1]", vec![field("items"), Index(0), Index(1)]),
        ];
        for (text, want) in cases {
            let got: JsonPath = text.parse().unwrap();
            assert_eq!(got.segments(), want, "{text}");
        }
    }

    #[test]
    fn invalid_paths_are_rejected() {
        for text in [
            "",
            "a",
            ".a",
            "$.",
            "$..a",
            "$.a.",
            "$a",
            "$.*",
            "$[*]",
            "$[-1]",
            "$[+1]",
            "$[]",
            "$[1",
            "$[１]",
            "$['a']",
            "$.a b",
            "$.é",
            "$[4294967296]",
            "$[?(@.x)]",
            "$.a[0]b",
            "$[0]x",
        ] {
            assert!(text.parse::<JsonPath>().is_err(), "{text}");
        }
    }
}
