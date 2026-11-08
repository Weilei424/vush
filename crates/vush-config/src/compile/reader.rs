//! A map reader that tracks used keys, and typed getters for single values.

use vush_core::plan::{JsonNumber, ValueError};

use super::suggest::closest;
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

fn conflict_message(earlier: &str, later: &str) -> String {
    format!("`{earlier}` and `{later}` can't be used together")
}

/// `unknown key` for `key`, with the closest of `choices` when one is near.
pub(super) fn unknown_key(key: &str, choices: impl IntoIterator<Item = &'static str>) -> String {
    let mut message = format!("unknown key `{}`", display_key(key));
    if let Some(choice) = closest(key, choices) {
        message.push_str(&format!(", did you mean `{choice}`?"));
    }
    message
}

/// Reads the keys of one map and reports the ones nobody asked for.
pub(super) struct MapReader<'a> {
    node: &'a Node,
    entries: &'a [Entry],
    used: Vec<bool>,
    /// Keys asked for, which are the ones offered as hints.
    asked: Vec<&'static str>,
    path: &'a str,
}

impl<'a> MapReader<'a> {
    pub(super) fn new(node: &'a Node, path: &'a str, errors: &mut Errors) -> Option<Self> {
        match &node.kind {
            NodeKind::Map(entries) => Some(MapReader {
                node,
                entries,
                used: vec![false; entries.len()],
                asked: Vec::new(),
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

    /// Marks the keys used, if present, without reading them.
    pub(super) fn ignore(&mut self, keys: &[&str]) {
        for (entry, used) in self.entries.iter().zip(self.used.iter_mut()) {
            if keys.contains(&entry.key.as_str()) {
                *used = true;
            }
        }
    }

    pub(super) fn optional(&mut self, key: &'static str) -> Option<&'a Node> {
        self.asked.push(key);
        let at = self.position(key)?;
        self.used[at] = true;
        Some(&self.entries[at].value)
    }

    /// Like `optional`, but the key is offered as a hint only while none of
    /// `conflicts` is in the map.
    pub(super) fn optional_unless(
        &mut self,
        key: &'static str,
        conflicts: &[&str],
    ) -> Option<&'a Node> {
        let found = self.optional(key);
        if conflicts.iter().any(|k| self.has(k)) {
            self.asked.pop();
        }
        found
    }

    pub(super) fn required(&mut self, key: &'static str, errors: &mut Errors) -> Option<&'a Node> {
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
        // A group with a member present can't be completed by a typo.
        if !keys.iter().any(|k| self.has(k)) {
            self.asked.extend_from_slice(keys);
        }
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
                    conflict_message(first, key),
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

    /// If both keys are present, marks `key` used (not `with`) and records a
    /// conflict at whichever of the two comes later in the map.
    pub(super) fn conflict(&mut self, key: &'static str, with: &'static str, errors: &mut Errors) {
        let (Some(a), Some(b)) = (self.position(key), self.position(with)) else {
            return;
        };
        self.used[a] = true;
        let (first, later, at) = if a < b {
            (key, with, b)
        } else {
            (with, key, a)
        };
        errors.push(
            &child(self.path, later),
            Some(self.entries[at].key_location),
            conflict_message(first, later),
        );
    }

    /// Records an unknown-key error for each key nobody used.
    pub(super) fn finish(self, errors: &mut Errors) {
        let mut offered: Option<Vec<&'static str>> = None;
        for (entry, used) in self.entries.iter().zip(&self.used) {
            if !used {
                errors.push_with(
                    &child(self.path, &entry.key),
                    Some(entry.key_location),
                    || {
                        let offered = offered.get_or_insert_with(|| {
                            self.asked
                                .iter()
                                .copied()
                                .filter(|k| !self.entries.iter().any(|e| e.key == *k))
                                .collect::<Vec<_>>()
                        });
                        unknown_key(&entry.key, offered.iter().copied())
                    },
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
    let text = match &node.kind {
        NodeKind::String(s) => s,
        NodeKind::Invalid => return None,
        _ => {
            // Empty text is wrong for every text form except a template, so
            // the parser's complaint names the form that was expected.
            let what = parse("").err().map_or("a string", |e| e.expected);
            wrong_type(node, path, what, errors);
            return None;
        }
    };
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
    let names: Vec<&str> = choices.iter().map(|(name, _)| *name).collect();
    let listed = match names.split_last() {
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    };
    let text = match &node.kind {
        NodeKind::String(s) => s,
        NodeKind::Invalid => return None,
        _ => {
            wrong_type(node, path, &format!("one of {listed}"), errors);
            return None;
        }
    };
    if let Some((_, value)) = choices.iter().find(|(name, _)| *name == text) {
        return Some(*value);
    }
    errors.push(
        path,
        Some(node.location),
        format!("expected one of {listed}"),
    );
    None
}

#[cfg(test)]
mod tests {
    use vush_core::plan::{
        ByteSize, HttpMethod, JsonPath, Rate, Ratio, Template, VarRef, parse_duration,
    };

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

    fn unknown_message(yaml: &str, ask: impl FnOnce(&mut MapReader<'_>, &mut Errors)) -> String {
        let (_, errors) = run(yaml, |node, errors| {
            let mut map = MapReader::new(node, "p", errors).unwrap();
            ask(&mut map, errors);
            map.finish(errors);
        });
        errors.last().unwrap().message.clone()
    }

    #[test]
    fn every_way_of_asking_records_the_key() {
        let hint = "unknown key `nmae`, did you mean `name`?";
        assert_eq!(
            unknown_message("{nmae: 1}", |m, _| {
                m.optional("name");
            }),
            hint
        );
        assert_eq!(
            unknown_message("{nmae: 1}", |m, e| {
                m.required("name", e);
            }),
            hint
        );
        assert_eq!(
            unknown_message("{nmae: 1}", |m, e| {
                m.at_most_one(&["name", "url"], e);
            }),
            hint
        );
        assert_eq!(
            unknown_message("{nmae: 1}", |m, e| {
                m.exactly_one(&["name", "url"], e);
            }),
            hint
        );
    }

    #[test]
    fn keys_already_in_the_map_are_not_offered() {
        let got = unknown_message("{name: a, nmae: b}", |m, _| {
            m.optional("name");
        });
        assert_eq!(got, "unknown key `nmae`");
    }

    #[test]
    fn keys_only_rejected_or_checked_are_not_offered() {
        let plain = "unknown key `nmae`";
        assert_eq!(
            unknown_message("{nmae: 1}", |m, e| m.reject("name", "no", e)),
            plain
        );
        assert_eq!(
            unknown_message("{nmae: 1}", |m, e| m.conflict("name", "url", e)),
            plain
        );
        assert_eq!(
            unknown_message("{nmae: 1}", |m, _| {
                m.has("name");
            }),
            plain
        );
        assert_eq!(
            unknown_message("{nmae: 1}", |m, _| m.ignore(&["name"])),
            plain
        );
    }

    #[test]
    fn a_cut_key_stays_cut_and_gets_no_hint() {
        let key = "k".repeat(70);
        let got = unknown_message(&format!("{{{key}: 1}}"), |m, _| {
            m.optional("name");
        });
        assert_eq!(got, format!("unknown key `{}…`", "k".repeat(64)));
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
    fn conflict_points_at_the_later_key() {
        let (_, errors) = run("{vus: 1, stages: []}", |node, errors| {
            let mut map = MapReader::new(node, "load", errors).unwrap();
            map.conflict("vus", "stages", errors);
        });
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "load.stages");
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
                .contains("`vus` and `stages` can't be used together")
        );

        let (_, errors) = run("{stages: [], vus: 1}", |node, errors| {
            let mut map = MapReader::new(node, "load", errors).unwrap();
            map.conflict("vus", "stages", errors);
        });
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "load.vus");
        assert!(
            errors[0]
                .message
                .contains("`stages` and `vus` can't be used together")
        );

        let (_, errors) = run("{vus: 1}", |node, errors| {
            let mut map = MapReader::new(node, "load", errors).unwrap();
            map.conflict("vus", "stages", errors);
            map.optional("vus");
            map.finish(errors);
        });
        assert!(errors.is_empty(), "{errors:?}");
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
        let never = |_: &str| -> Result<(), ValueError> { panic!("the parser must not run") };
        assert!(parsed(&invalid, "p", never, &mut errors).is_none());
        assert!(one_of(&invalid, "p", &[("a", 1)], &mut errors).is_none());
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
        assert!(
            errors[0]
                .message
                .contains("expected a duration such as 500ms")
        );
    }

    fn message<T>(yaml: &str, parse: impl FnOnce(&str) -> Result<T, ValueError>) -> String {
        let (_, errors) = run(yaml, |node, errors| {
            parsed(node, "p", parse, errors).is_some()
        });
        errors[0].message.clone()
    }

    #[test]
    fn wrong_types_name_the_text_form() {
        for (got, want) in [
            (
                message("5", parse_duration),
                "a duration such as 500ms, 5s, 5m or 1h30m",
            ),
            (
                message("5", str::parse::<Rate>),
                "a rate such as 20/s, 300/m or 5/h",
            ),
            (
                message("5", str::parse::<ByteSize>),
                "a size such as 512B, 500KB, 2MB or 1MiB",
            ),
            (
                message("5", str::parse::<Ratio>),
                "a percentage from 0% to 100% with up to 4 decimals, such as 1% or 0.5%",
            ),
            (
                message("5", str::parse::<HttpMethod>),
                "an HTTP method such as GET, POST or M-SEARCH (uppercase A-Z, 0-9 and -, starting with a letter)",
            ),
            (
                message("5", str::parse::<JsonPath>),
                "a JSON path such as $, $.name or $.items[0].id (names use A-Z, a-z, 0-9, _ and -)",
            ),
            (
                message("5", str::parse::<VarRef>),
                "a variable reference such as token, scenario.x, global.x, env.NAME, vu.id or vu.iteration",
            ),
            (message("5", str::parse::<Template>), "a string"),
        ] {
            assert_eq!(got, format!("expected {want}, found a number"));
        }
    }

    #[test]
    fn text_forms_say_what_was_found() {
        for (yaml, found) in [
            ("~", "null"),
            ("true", "a boolean"),
            ("[]", "a list"),
            ("{}", "a map"),
        ] {
            assert_eq!(
                message(yaml, parse_duration),
                format!("expected a duration such as 500ms, 5s, 5m or 1h30m, found {found}"),
                "{yaml}"
            );
        }
    }

    #[test]
    fn word_lists_name_the_choices_on_a_wrong_type() {
        let choices = [("auto", 1), ("http1", 2), ("http2", 3)];
        let (_, errors) = run("5", |node, errors| one_of(node, "p", &choices, errors));
        assert_eq!(
            errors[0].message,
            "expected one of auto, http1 or http2, found a number"
        );
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
