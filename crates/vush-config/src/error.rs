//! Errors that point at a place in the file.

use std::fmt;

use crate::MAX_ERRORS;

/// Both numbers start at 1. Columns count characters, not bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Location {
    pub line: u32,
    pub column: u32,
}

/// One problem found in a test file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// Empty when the problem isn't under a key.
    pub path: String,
    /// `None` only for problems with the file as a whole.
    pub location: Option<Location>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(at) = self.location {
            write!(f, "{}:{}: ", at.line, at.column)?;
        }
        if !self.path.is_empty() {
            write!(f, "{}: ", self.path)?;
        }
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Collects errors up to `MAX_ERRORS`, then remembers that some were dropped.
pub(crate) struct Errors {
    list: Vec<ConfigError>,
    overflowed: bool,
}

impl Errors {
    pub(crate) fn new() -> Self {
        Errors {
            list: Vec::new(),
            overflowed: false,
        }
    }

    pub(crate) fn push(
        &mut self,
        path: &str,
        location: Option<Location>,
        message: impl Into<String>,
    ) {
        if self.list.len() >= MAX_ERRORS {
            self.overflowed = true;
            return;
        }
        self.list.push(ConfigError {
            path: path.to_owned(),
            location,
            message: message.into(),
        });
    }

    /// Notes that errors were dropped before they reached this list, so the
    /// closing note is added.
    pub(crate) fn mark_overflowed(&mut self) {
        self.overflowed = true;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Errors in file order, with unlocated ones first, plus a closing note
    /// when some were dropped.
    pub(crate) fn into_sorted(mut self) -> Vec<ConfigError> {
        self.list.sort_by_key(|e| e.location);
        if self.overflowed {
            self.list.push(ConfigError {
                path: String::new(),
                location: None,
                message: format!("too many errors, only the first {MAX_ERRORS} are shown"),
            });
        }
        self.list
    }
}

const KEY_DISPLAY_LIMIT: usize = 64;

/// A key made safe to print: escaped, and cut after 64 characters.
pub(crate) fn display_key(key: &str) -> String {
    let mut out = String::new();
    push_display_key(&mut out, key, false);
    out
}

/// Appends [`display_key`]'s text for `key` to `out`. `cut` says the key is
/// already a cut prefix, so the text ends in one `…` even when the prefix fits.
pub(crate) fn push_display_key(out: &mut String, key: &str, cut: bool) {
    let mut chars = key.chars();
    for c in chars.by_ref().take(KEY_DISPLAY_LIMIT) {
        out.extend(c.escape_debug());
    }
    if cut || chars.next().is_some() {
        out.push('…');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_leaves_out_missing_parts() {
        let at = Some(Location { line: 3, column: 7 });
        for (path, location, want) in [
            ("a.b", at, "3:7: a.b: bad"),
            ("", at, "3:7: bad"),
            ("a.b", None, "a.b: bad"),
            ("", None, "bad"),
        ] {
            let error = ConfigError {
                path: path.to_owned(),
                location,
                message: "bad".to_owned(),
            };
            assert_eq!(error.to_string(), want);
        }
    }

    #[test]
    fn errors_sort_by_location_with_unlocated_first() {
        let mut errors = Errors::new();
        errors.push("", Some(Location { line: 2, column: 1 }), "c");
        errors.push("", Some(Location { line: 1, column: 9 }), "b");
        errors.push("", None, "a");
        errors.push("", Some(Location { line: 2, column: 1 }), "d");
        let messages: Vec<_> = errors
            .into_sorted()
            .into_iter()
            .map(|e| e.message)
            .collect();
        assert_eq!(messages, ["a", "b", "c", "d"]);
    }

    #[test]
    fn errors_keep_the_cap_then_add_one_note() {
        let mut errors = Errors::new();
        for _ in 0..MAX_ERRORS {
            errors.push("", None, "x");
        }
        assert_eq!(errors.into_sorted().len(), MAX_ERRORS);

        let mut errors = Errors::new();
        for _ in 0..MAX_ERRORS + 50 {
            errors.push("", None, "x");
        }
        let list = errors.into_sorted();
        assert_eq!(list.len(), MAX_ERRORS + 1);
        assert!(list[MAX_ERRORS].message.contains("too many errors"));
    }

    #[test]
    fn keys_are_escaped_and_cut() {
        assert_eq!(display_key("plain-key_1"), "plain-key_1");
        assert_eq!(display_key("a\nb"), "a\\nb");
        assert_eq!(display_key(&"k".repeat(64)), "k".repeat(64));
        assert_eq!(display_key(&"k".repeat(65)), format!("{}…", "k".repeat(64)));
        assert_eq!(display_key(&"é".repeat(65)), format!("{}…", "é".repeat(64)));
    }
}
