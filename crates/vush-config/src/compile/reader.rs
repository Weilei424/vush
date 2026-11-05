//! A map reader that tracks used keys, and typed getters for single values.

use vush_core::plan::{JsonNumber, ValueError};

use crate::error::{Errors, display_key};
use crate::node::{Entry, Node, NodeKind};

/// Joins a map key onto a path.
pub(super) fn child(path: &str, key: &str) -> String {
    let key = display_key(key);
    if path.is_empty() {
        key
    } else {
        format!("{path}.{key}")
    }
}

/// Joins a list index onto a path.
pub(super) fn item(path: &str, index: usize) -> String {
    format!("{path}[{index}]")
}

fn type_name(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Null => "null",
        NodeKind::Bool(_) => "a boolean",
        NodeKind::Number(_) => "a number",
        NodeKind::String(_) => "a string",
        NodeKind::List(_) => "a list",
        NodeKind::Map(_) => "a map",
        NodeKind::Invalid => "an invalid value",
    }
}

/// Records `expected {what}, found {type}` at the node.
fn wrong_type(node: &Node, path: &str, what: &str, errors: &mut Errors) {
    errors.push(
        path,
        Some(node.location),
        format!("expected {what}, found {}", type_name(&node.kind)),
    );
}

fn join_keys(keys: &[&str]) -> String {
    let quoted: Vec<String> = keys.iter().map(|k| format!("`{k}`")).collect();
    match quoted.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// Reads the keys of one map and reports the ones nobody asked for.
pub(super) struct MapReader<'a> {
    node: &'a Node,
    entries: &'a [Entry],
    used: Vec<bool>,
    path: &'a str,
}

impl<'a> MapReader<'a> {
    pub(super) fn new(node: &'a Node, path: &'a str, errors: &mut Errors) -> Option<Self> {
        match &node.kind {
            NodeKind::Map(entries) => Some(MapReader {
                node,
                entries,
                used: vec![false; entries.len()],
                path,
            }),
            NodeKind::Invalid => None,
            _ => {
                wrong_type(node, path, "a map", errors);
                None
            }
        }
    }

    pub(super) fn path(&self) -> &'a str {
        self.path
    }

    fn position(&self, key: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.key == key)
    }

    /// Whether the key is present. Doesn't mark it used.
    pub(super) fn has(&self, key: &str) -> bool {
        self.position(key).is_some()
    }

    pub(super) fn optional(&mut self, key: &str) -> Option<&'a Node> {
        let at = self.position(key)?;
        self.used[at] = true;
        Some(&self.entries[at].value)
    }

    pub(super) fn required(&mut self, key: &str, errors: &mut Errors) -> Option<&'a Node> {
        let found = self.optional(key);
        if found.is_none() {
            errors.push(
                self.path,
                Some(self.node.location),
                format!("missing required key `{}`", display_key(key)),
            );
        }
        found
    }

    /// The key written first in the file wins. Each later present key is a conflict.
    pub(super) fn at_most_one(
        &mut self,
        keys: &[&'static str],
        errors: &mut Errors,
    ) -> Option<(&'static str, &'a Node)> {
        let mut winner: Option<(&'static str, &'a Node)> = None;
        for at in 0..self.entries.len() {
            let entry = &self.entries[at];
            let Some(&key) = keys.iter().find(|&&k| k == entry.key) else {
                continue;
            };
            self.used[at] = true;
            match winner {
                None => winner = Some((key, &entry.value)),
                Some((first, _)) => errors.push(
                    &child(self.path, key),
                    Some(entry.key_location),
                    format!("`{first}` and `{key}` can't be used together"),
                ),
            }
        }
        winner
    }

    /// Like `at_most_one`, and also reports when none is present.
    pub(super) fn exactly_one(
        &mut self,
        keys: &[&'static str],
        errors: &mut Errors,
    ) -> Option<(&'static str, &'a Node)> {
        let found = self.at_most_one(keys, errors);
        if found.is_none() {
            errors.push(
                self.path,
                Some(self.node.location),
                format!("needs one of {}", join_keys(keys)),
            );
        }
        found
    }

    /// If the key is present, marks it used and records `message` at the key.
    pub(super) fn reject(&mut self, key: &str, message: &str, errors: &mut Errors) {
        if let Some(at) = self.position(key) {
            self.used[at] = true;
            errors.push(
                &child(self.path, key),
                Some(self.entries[at].key_location),
                message,
            );
        }
    }

    /// Records an unknown-key error for each key nobody used.
    pub(super) fn finish(self, errors: &mut Errors) {
        for (entry, used) in self.entries.iter().zip(&self.used) {
            if !used {
                errors.push(
                    &child(self.path, &entry.key),
                    Some(entry.key_location),
                    format!("unknown key `{}`", display_key(&entry.key)),
                );
            }
        }
    }
}

pub(super) fn string<'a>(node: &'a Node, path: &str, errors: &mut Errors) -> Option<&'a str> {
    match &node.kind {
        NodeKind::String(s) => Some(s),
        NodeKind::Invalid => None,
        _ => {
            wrong_type(node, path, "a string", errors);
            None
        }
    }
}

pub(super) fn boolean(node: &Node, path: &str, errors: &mut Errors) -> Option<bool> {
    match &node.kind {
        NodeKind::Bool(b) => Some(*b),
        NodeKind::Invalid => None,
        _ => {
            wrong_type(node, path, "a boolean", errors);
            None
        }
    }
}

pub(super) fn number(node: &Node, path: &str, errors: &mut Errors) -> Option<JsonNumber> {
    match &node.kind {
        NodeKind::Number(n) => Some(*n),
        NodeKind::Invalid => None,
        _ => {
            wrong_type(node, path, "a number", errors);
            None
        }
    }
}

pub(super) fn integer(
    node: &Node,
    path: &str,
    min: u64,
    max: u64,
    errors: &mut Errors,
) -> Option<u64> {
    let what = format!("an integer from {min} to {max}");
    let value = match &node.kind {
        NodeKind::Number(JsonNumber::Int(n)) => u64::try_from(*n).ok(),
        NodeKind::Number(JsonNumber::UInt(n)) => Some(*n),
        NodeKind::Number(JsonNumber::Float(_)) => None,
        NodeKind::Invalid => return None,
        _ => {
            wrong_type(node, path, &what, errors);
            return None;
        }
    };
    match value {
        Some(n) if (min..=max).contains(&n) => Some(n),
        _ => {
            errors.push(path, Some(node.location), format!("expected {what}"));
            None
        }
    }
}

pub(super) fn list<'a>(node: &'a Node, path: &str, errors: &mut Errors) -> Option<&'a [Node]> {
    match &node.kind {
        NodeKind::List(items) => Some(items),
        NodeKind::Invalid => None,
        _ => {
            wrong_type(node, path, "a list", errors);
            None
        }
    }
}

/// Reads a string and parses it. The error names what's expected, never the text.
pub(super) fn parsed<T>(
    node: &Node,
    path: &str,
    parse: impl FnOnce(&str) -> Result<T, ValueError>,
    errors: &mut Errors,
) -> Option<T> {
    let text = string(node, path, errors)?;
    match parse(text) {
        Ok(value) => Some(value),
        Err(e) => {
            errors.push(
                path,
                Some(node.location),
                format!("expected {}", e.expected),
            );
            None
        }
    }
}

/// Reads a string that must be one of the listed names.
pub(super) fn one_of<T: Copy>(
    node: &Node,
    path: &str,
    choices: &[(&str, T)],
    errors: &mut Errors,
) -> Option<T> {
    let text = string(node, path, errors)?;
    if let Some((_, value)) = choices.iter().find(|(name, _)| *name == text) {
        return Some(*value);
    }
    let names: Vec<&str> = choices.iter().map(|(name, _)| *name).collect();
    let listed = match names.split_last() {
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    };
    errors.push(
        path,
        Some(node.location),
        format!("expected one of {listed}"),
    );
    None
}

#[cfg(test)]
mod tests {
    use vush_core::plan::parse_duration;

    use super::*;
    use crate::compile::testing::run;
    use crate::error::Location;

    #[test]
    fn paths_join_and_escape() {
        assert_eq!(child("", "a"), "a");
        assert_eq!(child("a", "b"), "a.b");
        assert_eq!(item("a", 2), "a[2]");
        assert_eq!(child("h", "x\ny"), "h.x\\ny");
    }

    #[test]
    fn unused_keys_are_unknown_at_the_key() {
        let (_, errors) = run("{a: 1, nmae: 2}", |node, errors| {
            let mut map = MapReader::new(node, "http", errors).unwrap();
            map.optional("a");
            map.finish(errors);
        });
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, "http.nmae");
        assert_eq!(errors[0].location, Some(Location { line: 1, column: 8 }));
        assert!(errors[0].message.contains("unknown key `nmae`"));
    }

    #[test]
    fn missing_keys_are_reported_at_the_map() {
        let (_, errors) = run("\n{a: 1}", |node, errors| {
            let mut map = MapReader::new(node, "http", errors).unwrap();
            map.required("name", errors);
        });
        assert_eq!(errors[0].path, "http");
        assert_eq!(errors[0].location, Some(Location { line: 2, column: 1 }));
        assert!(errors[0].message.contains("missing required key `name`"));
    }

    #[test]
    fn exactly_one_reports_none_and_conflicts() {
        let keys = ["path", "url"];
        let (_, errors) = run("{a: 1}", |node, errors| {
            let mut map = MapReader::new(node, "r", errors).unwrap();
            assert!(map.exactly_one(&keys, errors).is_none());
        });
        assert_eq!(errors[0].path, "r");
        assert!(errors[0].message.contains("needs one of `path` or `url`"));

        let (found, errors) = run("{path: a, url: b}", |node, errors| {
            let mut map = MapReader::new(node, "r", errors).unwrap();
            map.exactly_one(&keys, errors).map(|(key, _)| key)
        });
        assert_eq!(found, Some("path"));
        assert_eq!(errors[0].path, "r.url");
        assert!(
            errors[0]
                .message
                .contains("`path` and `url` can't be used together")
        );
    }

    #[test]
    fn conflicts_follow_document_order() {
        let (found, errors) = run("{url: b, path: a}", |node, errors| {
            let mut map = MapReader::new(node, "r", errors).unwrap();
            map.exactly_one(&["path", "url"], errors)
                .map(|(key, _)| key)
        });
        assert_eq!(found, Some("url"));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "r.path");
        assert_eq!(
            errors[0].location,
            Some(Location {
                line: 1,
                column: 10
            })
        );
        assert!(
            errors[0]
                .message
                .contains("`url` and `path` can't be used together")
        );
    }

    #[test]
    fn wrong_types_name_what_was_found() {
        for (yaml, found) in [
            ("~", "null"),
            ("true", "a boolean"),
            ("1", "a number"),
            ("[]", "a list"),
            ("{}", "a map"),
        ] {
            let (_, errors) = run(yaml, |node, errors| {
                string(node, "p", errors).map(str::to_owned)
            });
            assert_eq!(
                errors[0].message,
                format!("expected a string, found {found}"),
                "{yaml}"
            );
        }
        let (_, errors) = run("x", |node, errors| boolean(node, "p", errors));
        assert!(
            errors[0]
                .message
                .contains("expected a boolean, found a string")
        );
    }

    #[test]
    fn invalid_nodes_add_no_second_error() {
        let invalid = Node {
            location: Location { line: 1, column: 1 },
            kind: NodeKind::Invalid,
        };
        let mut errors = Errors::new();
        assert!(string(&invalid, "p", &mut errors).is_none());
        assert!(integer(&invalid, "p", 0, 9, &mut errors).is_none());
        assert!(MapReader::new(&invalid, "p", &mut errors).is_none());
        assert!(errors.is_empty());
    }

    #[test]
    fn integers_must_be_written_as_integers_in_range() {
        let (value, errors) = run("5", |node, errors| integer(node, "p", 1, 9, errors));
        assert_eq!((value, errors.len()), (Some(5), 0));
        for yaml in ["0", "10", "-1", "2.0", "1e3", "x"] {
            let (value, errors) = run(yaml, |node, errors| integer(node, "p", 1, 9, errors));
            assert_eq!(value, None, "{yaml}");
            assert!(
                errors[0]
                    .message
                    .contains("expected an integer from 1 to 9"),
                "{yaml}"
            );
        }
    }

    #[test]
    fn parsed_uses_the_expected_text_and_never_the_value() {
        let (_, errors) = run("banana", |node, errors| {
            parsed(node, "p", parse_duration, errors)
        });
        assert!(
            errors[0]
                .message
                .contains("expected a duration such as 500ms")
        );
        assert!(!errors[0].message.contains("banana"));
        let (_, errors) = run("5", |node, errors| {
            parsed(node, "p", parse_duration, errors)
        });
        assert!(errors[0].message.contains("expected a string"));
    }

    #[test]
    fn one_of_lists_the_choices() {
        let choices = [("auto", 1), ("http1", 2), ("http2", 3)];
        let (value, _) = run("http1", |node, errors| one_of(node, "p", &choices, errors));
        assert_eq!(value, Some(2));
        let (_, errors) = run("http3", |node, errors| one_of(node, "p", &choices, errors));
        assert!(
            errors[0]
                .message
                .contains("expected one of auto, http1 or http2")
        );
        assert!(!errors[0].message.contains("http3"));
    }
}
