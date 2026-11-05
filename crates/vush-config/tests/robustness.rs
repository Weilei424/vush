//! Cut and mutated files return cleanly, with a sane number of errors.

mod common;

use common::example_block;
use vush_config::{Format, MAX_ERRORS, parse};

const TOKENS: &[&str] = &[
    "&a ",
    "*a",
    "!!str ",
    "!<x> ",
    "<<: ",
    "? ",
    ": ",
    "- ",
    "{",
    "}",
    "[",
    "]",
    ",",
    "'",
    "\"",
    "|",
    "|-",
    ">",
    "#",
    "%YAML 1.2\n",
    "---\n",
    "...\n",
    "\t",
    "\r",
    "\n",
    "\r\n",
    "\u{feff}",
    "\u{85}",
    "\u{2028}",
    "\\u",
    "\\U0010FFFF",
    "\\",
    "\u{0}",
    "é",
    "日本語",
    "😀",
    "~",
    "${",
    "        ",
];

/// Every JSON escape, a surrogate pair, raw multibyte text, every number form,
/// nested containers and CRLF line endings.
const JSON_SEED: &str = "{\r\n  \"name\": \"\\\" \\\\ \\/ \\b \\f \\n \\r \\t \\u00e9 \\ud83d\\ude00 é 日本語 😀\",\r\n  \"numbers\": [0, -0, 12, -12, 1.5, -0.25, 1e3, 1E+2, 1e-400, 9223372036854775807, 18446744073709551615, -9223372036854775808],\r\n  \"nested\": {\"a\": [[], {}, [{\"b\": [null, true, false]}]]},\r\n  \"empty\": \"\"\r\n}\r\n";

/// Double-quoted escapes, a single-quoted `''`, a `|` block, a `>-` block, an
/// anchor, an alias, a merge key and CRLF line endings.
const YAML_SEED: &str = "base: &base\r\n  a: 1\r\n  b: two\r\ndouble: \"tab\\t hex\\x41 é \\U0001F600 \\u00e9 \\\\ \\\"\"\r\nsingle: 'it''s'\r\nliteral: |\r\n  line one\r\n  line two\r\nfolded: >-\r\n  folded one\r\n  folded two\r\nalias: *base\r\nmerged:\r\n  <<: *base\r\n  c: 3\r\n";

fn check(text: &str, format: Format) {
    if let Err(errors) = parse(text, format) {
        assert!(
            (1..=MAX_ERRORS + 1).contains(&errors.len()),
            "{} errors for {text:?}",
            errors.len()
        );
    }
}

fn seeds() -> [(&'static str, Format); 2] {
    [
        (example_block("yaml"), Format::Yaml),
        (example_block("json"), Format::Json),
    ]
}

#[test]
fn truncated_examples_return_cleanly() {
    for (text, format) in seeds() {
        for end in (0..=text.len())
            .step_by(61)
            .filter(|&i| text.is_char_boundary(i))
        {
            check(&text[..end], format);
        }
    }
}

#[test]
fn escape_heavy_documents_truncated_at_every_character_return_cleanly() {
    for (text, format) in [(JSON_SEED, Format::Json), (YAML_SEED, Format::Yaml)] {
        check(text, format);
        for (end, _) in text.char_indices() {
            check(&text[..end], format);
        }
    }
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

#[test]
fn mutated_examples_return_cleanly() {
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    for (seed, format) in seeds() {
        for _ in 0..150 {
            let mut chars: Vec<char> = seed.chars().collect();
            for _ in 0..1 + rng.below(8) {
                let at = rng.below(chars.len() + 1);
                match rng.below(3) {
                    0 => {
                        let token = TOKENS[rng.below(TOKENS.len())];
                        chars.splice(at..at, token.chars());
                    }
                    1 => {
                        let token = TOKENS[rng.below(TOKENS.len())];
                        let end = (at + token.chars().count()).min(chars.len());
                        chars.splice(at..end, token.chars());
                    }
                    _ => {
                        let end = (at + 1 + rng.below(40)).min(chars.len());
                        chars.drain(at..end);
                    }
                }
            }
            let mutated: String = chars.into_iter().collect();
            check(&mutated, format);
        }
    }
}
