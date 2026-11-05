//! The located value tree both readers produce, and the reading budget.

use std::collections::HashMap;

use vush_core::plan::JsonNumber;

use crate::error::{Errors, Location, display_key};
use crate::{MAX_DEPTH, MAX_TEXT_BYTES, MAX_VALUES};

/// Message for a file with nothing in it. Both readers use it.
pub(crate) const EMPTY_FILE: &str = "the file is empty";

/// Message for nesting past `MAX_DEPTH`. Both readers use it.
pub(crate) fn depth_message() -> String {
    format!("nesting is deeper than the limit of {MAX_DEPTH} levels")
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Node {
    pub(crate) location: Location,
    pub(crate) kind: NodeKind,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NodeKind {
    Null,
    Bool(bool),
    Number(JsonNumber),
    String(String),
    List(Vec<Node>),
    Map(Vec<Entry>),
    /// A value the reader already reported an error for.
    Invalid,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub(crate) key: String,
    pub(crate) key_location: Location,
    pub(crate) value: Node,
}

/// Counts values and text so aliases and huge files can't blow up memory.
pub(crate) struct Budget {
    values: usize,
    text: usize,
}

impl Budget {
    pub(crate) fn new() -> Self {
        Budget { values: 0, text: 0 }
    }

    /// Adds to the running totals. The `Err` is the message for the limit hit.
    pub(crate) fn charge(&mut self, values: usize, text: usize) -> Result<(), &'static str> {
        self.values = self.values.saturating_add(values);
        self.text = self.text.saturating_add(text);
        if self.values > MAX_VALUES {
            return Err(
                "the file holds more than the limit of 250000 values, counting alias copies",
            );
        }
        if self.text > MAX_TEXT_BYTES {
            return Err(
                "the file holds more than the limit of 64 MiB of text, counting alias copies",
            );
        }
        Ok(())
    }
}

/// Keeps the first entry of each key, in order, and reports the later ones.
pub(crate) fn dedupe_keys(entries: Vec<Entry>, errors: &mut Errors) -> Vec<Entry> {
    let mut first_line: HashMap<String, u32> = HashMap::new();
    let mut kept = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(line) = first_line.get(&entry.key) {
            errors.push(
                "",
                Some(entry.key_location),
                format!(
                    "duplicate key `{}`, first used on line {line}",
                    display_key(&entry.key)
                ),
            );
        } else {
            first_line.insert(entry.key.clone(), entry.key_location.line);
            kept.push(entry);
        }
    }
    kept
}

#[cfg(test)]
impl Node {
    /// A compact rendering for tests.
    pub(crate) fn shape(&self) -> String {
        match &self.kind {
            NodeKind::Null => "null".to_owned(),
            NodeKind::Bool(b) => b.to_string(),
            NodeKind::Number(JsonNumber::Int(n)) => n.to_string(),
            NodeKind::Number(JsonNumber::UInt(n)) => n.to_string(),
            NodeKind::Number(JsonNumber::Float(n)) => format!("{n:?}"),
            NodeKind::String(s) => format!("{s:?}"),
            NodeKind::List(items) => {
                let parts: Vec<_> = items.iter().map(Node::shape).collect();
                format!("[{}]", parts.join(","))
            }
            NodeKind::Map(entries) => {
                let parts: Vec<_> = entries
                    .iter()
                    .map(|e| format!("{}:{}", e.key, e.value.shape()))
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
            NodeKind::Invalid => "!".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_allows_the_limit_and_refuses_one_more() {
        let mut budget = Budget::new();
        assert!(budget.charge(MAX_VALUES, 0).is_ok());
        assert!(budget.charge(1, 0).unwrap_err().contains("250000 values"));

        let mut budget = Budget::new();
        assert!(budget.charge(0, MAX_TEXT_BYTES).is_ok());
        assert!(budget.charge(0, 1).unwrap_err().contains("64 MiB of text"));
    }

    #[test]
    fn duplicate_keys_keep_the_first_and_report_the_rest() {
        let entry = |key: &str, line| Entry {
            key: key.to_owned(),
            key_location: Location { line, column: 1 },
            value: Node {
                location: Location { line, column: 4 },
                kind: NodeKind::Null,
            },
        };
        let mut errors = Errors::new();
        let kept = dedupe_keys(
            vec![entry("a", 1), entry("b", 2), entry("a", 3), entry("a", 4)],
            &mut errors,
        );
        assert_eq!(
            kept.iter()
                .map(|e| (e.key.as_str(), e.key_location.line))
                .collect::<Vec<_>>(),
            [("a", 1), ("b", 2)]
        );
        let list = errors.into_sorted();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].location, Some(Location { line: 3, column: 1 }));
        assert!(
            list[0]
                .message
                .contains("duplicate key `a`, first used on line 1")
        );
    }

    #[test]
    fn shape_renders_each_kind() {
        let at = Location { line: 1, column: 1 };
        let node = |kind| Node { location: at, kind };
        let entry = |key: &str, value| Entry {
            key: key.to_owned(),
            key_location: at,
            value,
        };
        let tree = node(NodeKind::Map(vec![
            entry("a", node(NodeKind::Null)),
            entry(
                "b",
                node(NodeKind::List(vec![
                    node(NodeKind::Bool(true)),
                    node(NodeKind::Number(JsonNumber::Int(2))),
                    node(NodeKind::Number(JsonNumber::UInt(u64::MAX))),
                    node(NodeKind::Number(JsonNumber::Float(2.5))),
                    node(NodeKind::String("x".to_owned())),
                    node(NodeKind::Invalid),
                ])),
            ),
        ]));
        assert_eq!(
            tree.shape(),
            r#"{a:null,b:[true,2,18446744073709551615,2.5,"x",!]}"#
        );
    }
}
