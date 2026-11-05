//! A YAML reader built on `saphyr-parser` events. It produces the same located
//! tree as the JSON reader, resolves anchors, aliases and merge keys, and never
//! copies text from the file into an error message.

use std::collections::{HashMap, HashSet};

use saphyr_parser::{Event, Marker, Parser, ScalarStyle, StrInput};

use crate::MAX_DEPTH;
use crate::error::{Errors, Location, display_key};
use crate::node::{Budget, EMPTY_FILE, Entry, Node, NodeKind, dedupe_keys, depth_message};
use crate::scalar::{AMBIGUOUS_NUMBER, Plain, classify_plain};

const TAG_MESSAGE: &str = "YAML tags aren't supported";
const KEY_MESSAGE: &str = "keys must be strings";
const MERGE_MESSAGE: &str = "`<<` needs a map or a list of maps";
const SELF_ALIAS_MESSAGE: &str = "an alias can't refer to the value that contains it";

/// Reads `text` into a tree. `None` means a fatal problem was recorded.
pub(crate) fn read(text: &str, errors: &mut Errors) -> Option<Node> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut reader = Reader {
        parser: Parser::new_from_str(text),
        budget: Budget::new(),
        anchors: HashMap::new(),
        errors,
    };
    reader.document().ok()
}

/// A fatal problem, already recorded.
struct Fatal;

/// A finished anchored value, with what a copy of it costs.
struct Anchor {
    node: Node,
    values: usize,
    text: usize,
    height: usize,
}

/// A `<<` entry waiting for its map to finish.
struct Merge {
    /// How many ordinary entries were written before it.
    position: usize,
    key_location: Location,
    value: Node,
}

struct Reader<'a> {
    parser: Parser<'a, StrInput<'a>>,
    budget: Budget,
    anchors: HashMap<usize, Anchor>,
    errors: &'a mut Errors,
}

fn location(marker: Marker) -> Location {
    Location {
        line: u32::try_from(marker.line()).unwrap_or(u32::MAX),
        column: u32::try_from(marker.col().saturating_add(1)).unwrap_or(u32::MAX),
    }
}

/// Values, bytes of text and levels of maps and lists in a finished node.
fn measure(node: &Node) -> (usize, usize, usize) {
    match &node.kind {
        NodeKind::String(text) => (1, text.len(), 0),
        NodeKind::List(items) => {
            let mut total = (1, 0, 0);
            let mut height = 0;
            for item in items {
                let (values, text, h) = measure(item);
                total.0 += values;
                total.1 += text;
                height = height.max(h);
            }
            (total.0, total.1, height + 1)
        }
        NodeKind::Map(entries) => {
            let mut total = (1, 0, 0);
            let mut height = 0;
            for entry in entries {
                let (values, text, h) = measure(&entry.value);
                total.0 += values;
                total.1 += text + entry.key.len();
                height = height.max(h);
            }
            (total.0, total.1, height + 1)
        }
        _ => (1, 0, 0),
    }
}

impl<'a> Reader<'a> {
    fn fatal<T>(&mut self, at: Option<Location>, message: &str) -> Result<T, Fatal> {
        self.errors.push("", at, message);
        Err(Fatal)
    }

    fn problem(&mut self, at: Location, message: &str) {
        self.errors.push("", Some(at), message);
    }

    /// The next event, skipping `Nothing`. A parser error is fatal.
    fn next(&mut self) -> Result<(Event<'a>, Location), Fatal> {
        loop {
            match self.parser.next_event() {
                None => return self.fatal(None, "the file ends before the YAML is complete"),
                Some(Err(error)) => {
                    let at = location(*error.marker());
                    return self.fatal(Some(at), error.info());
                }
                Some(Ok((Event::Nothing, _))) => {}
                Some(Ok((event, span))) => return Ok((event, location(span.start))),
            }
        }
    }

    fn charge(&mut self, at: Location, values: usize, text: usize) -> Result<(), Fatal> {
        match self.budget.charge(values, text) {
            Ok(()) => Ok(()),
            Err(message) => self.fatal(Some(at), message),
        }
    }

    fn document(&mut self) -> Result<Node, Fatal> {
        let (event, _) = self.next()?;
        if !matches!(event, Event::StreamStart) {
            return self.fatal(None, "unexpected YAML structure");
        }
        let (event, at) = self.next()?;
        match event {
            Event::StreamEnd => return self.fatal(None, EMPTY_FILE),
            Event::DocumentStart(_) => {}
            _ => return self.fatal(Some(at), "unexpected YAML structure"),
        }
        let (event, at) = self.next()?;
        if matches!(event, Event::DocumentEnd | Event::StreamEnd) {
            return self.fatal(None, EMPTY_FILE);
        }
        let node = self.node(event, at, 1)?;
        let (event, at) = self.next()?;
        if !matches!(event, Event::DocumentEnd) {
            return self.fatal(Some(at), "unexpected YAML structure");
        }
        let (event, at) = self.next()?;
        match event {
            Event::StreamEnd => Ok(node),
            Event::DocumentStart(_) => {
                self.fatal(Some(at), "only one YAML document is allowed per file")
            }
            _ => self.fatal(Some(at), "unexpected YAML structure"),
        }
    }

    /// Builds the node an event starts. `level` is the level it would sit at
    /// if it is a map or a list.
    fn node(&mut self, event: Event<'a>, at: Location, level: usize) -> Result<Node, Fatal> {
        match event {
            Event::Scalar(text, style, anchor, tag) => {
                self.scalar(&text, style, anchor, tag.is_some(), at)
            }
            Event::Alias(id) => self.alias(id, at, level),
            Event::SequenceStart(anchor, tag) => self.list(anchor, tag.is_some(), at, level),
            Event::MappingStart(anchor, tag) => self.map(anchor, tag.is_some(), at, level),
            _ => self.fatal(Some(at), "unexpected YAML structure"),
        }
    }

    fn scalar(
        &mut self,
        text: &str,
        style: ScalarStyle,
        anchor: usize,
        tagged: bool,
        at: Location,
    ) -> Result<Node, Fatal> {
        let mut text_bytes = 0;
        let kind = if tagged {
            self.problem(at, TAG_MESSAGE);
            NodeKind::Invalid
        } else if style != ScalarStyle::Plain {
            text_bytes = text.len();
            NodeKind::String(text.to_owned())
        } else {
            match classify_plain(text) {
                Plain::Null => NodeKind::Null,
                Plain::Bool(b) => NodeKind::Bool(b),
                Plain::Number(n) => NodeKind::Number(n),
                Plain::BadNumber(problem) => {
                    self.problem(at, problem.message());
                    NodeKind::Invalid
                }
                Plain::Ambiguous => {
                    self.problem(at, AMBIGUOUS_NUMBER);
                    NodeKind::Invalid
                }
                Plain::String => {
                    text_bytes = text.len();
                    NodeKind::String(text.to_owned())
                }
            }
        };
        self.charge(at, 1, text_bytes)?;
        let node = Node { location: at, kind };
        self.register(anchor, &node, at)?;
        Ok(node)
    }

    /// Stores a finished anchored node and charges the budget for the copy.
    fn register(&mut self, anchor: usize, node: &Node, at: Location) -> Result<(), Fatal> {
        if anchor == 0 {
            return Ok(());
        }
        let (values, text, height) = measure(node);
        self.charge(at, values, text)?;
        self.anchors.insert(
            anchor,
            Anchor {
                node: node.clone(),
                values,
                text,
                height,
            },
        );
        Ok(())
    }

    fn alias(&mut self, id: usize, at: Location, level: usize) -> Result<Node, Fatal> {
        let Some((values, text, height)) =
            self.anchors.get(&id).map(|a| (a.values, a.text, a.height))
        else {
            self.problem(at, SELF_ALIAS_MESSAGE);
            self.charge(at, 1, 0)?;
            return Ok(Node {
                location: at,
                kind: NodeKind::Invalid,
            });
        };
        if level.saturating_sub(1) + height > MAX_DEPTH {
            return self.fatal(Some(at), &depth_message());
        }
        self.charge(at, values, text)?;
        match self.anchors.get(&id) {
            Some(anchor) => Ok(anchor.node.clone()),
            None => self.fatal(Some(at), SELF_ALIAS_MESSAGE),
        }
    }

    fn enter(&mut self, level: usize, at: Location) -> Result<(), Fatal> {
        if level > MAX_DEPTH {
            return self.fatal(Some(at), &depth_message());
        }
        self.charge(at, 1, 0)
    }

    fn list(
        &mut self,
        anchor: usize,
        tagged: bool,
        at: Location,
        level: usize,
    ) -> Result<Node, Fatal> {
        self.enter(level, at)?;
        let mut items = Vec::new();
        loop {
            let (event, item_at) = self.next()?;
            if matches!(event, Event::SequenceEnd) {
                break;
            }
            items.push(self.node(event, item_at, level + 1)?);
        }
        let kind = if tagged {
            self.problem(at, TAG_MESSAGE);
            NodeKind::Invalid
        } else {
            NodeKind::List(items)
        };
        let node = Node { location: at, kind };
        self.register(anchor, &node, at)?;
        Ok(node)
    }

    fn map(
        &mut self,
        anchor: usize,
        tagged: bool,
        at: Location,
        level: usize,
    ) -> Result<Node, Fatal> {
        self.enter(level, at)?;
        let mut entries: Vec<Entry> = Vec::new();
        let mut merge: Option<Merge> = None;
        loop {
            let (event, key_at) = self.next()?;
            match event {
                Event::MappingEnd => break,
                Event::Scalar(text, style, key_anchor, tag) => {
                    if tag.is_some() {
                        self.problem(key_at, TAG_MESSAGE);
                    }
                    self.charge(key_at, 0, text.len())?;
                    let key_node = Node {
                        location: key_at,
                        kind: NodeKind::String(text.to_string()),
                    };
                    self.register(key_anchor, &key_node, key_at)?;
                    let (value_event, value_at) = self.next()?;
                    let value = self.node(value_event, value_at, level + 1)?;
                    if style == ScalarStyle::Plain && text == "<<" {
                        match &merge {
                            Some(first) => {
                                let line = first.key_location.line;
                                self.problem(
                                    key_at,
                                    &format!(
                                        "duplicate key `{}`, first used on line {line}",
                                        display_key("<<")
                                    ),
                                );
                            }
                            None => {
                                merge = Some(Merge {
                                    position: entries.len(),
                                    key_location: key_at,
                                    value,
                                });
                            }
                        }
                    } else {
                        entries.push(Entry {
                            key: text.into_owned(),
                            key_location: key_at,
                            value,
                        });
                    }
                }
                other => {
                    // The key and its value are still read, so limits apply.
                    self.node(other, key_at, level + 1)?;
                    self.problem(key_at, KEY_MESSAGE);
                    let (value_event, value_at) = self.next()?;
                    self.node(value_event, value_at, level + 1)?;
                }
            }
        }
        // Every kept entry before `<<` is the first use of its key, so the
        // number of distinct keys written before it is where it lands.
        let position = merge.as_ref().map(|m| {
            let before = entries.get(..m.position).unwrap_or(&entries);
            before
                .iter()
                .map(|e| e.key.as_str())
                .collect::<HashSet<_>>()
                .len()
        });
        let mut entries = dedupe_keys(entries, self.errors);
        if let (Some(merge), Some(position)) = (merge, position) {
            let merged = self.merged_entries(merge.value, &entries);
            let tail = entries.split_off(position.min(entries.len()));
            entries.extend(merged);
            entries.extend(tail);
        }
        let kind = if tagged {
            self.problem(at, TAG_MESSAGE);
            NodeKind::Invalid
        } else {
            NodeKind::Map(entries)
        };
        let node = Node { location: at, kind };
        self.register(anchor, &node, at)?;
        Ok(node)
    }

    /// The entries a `<<` value brings in, minus keys the map writes itself
    /// and keys an earlier source already brought.
    fn merged_entries(&mut self, value: Node, own: &[Entry]) -> Vec<Entry> {
        let mut sources = Vec::new();
        match value.kind {
            NodeKind::Map(entries) => sources.push(entries),
            NodeKind::List(items) => {
                for item in items {
                    match item.kind {
                        NodeKind::Map(entries) => sources.push(entries),
                        NodeKind::Invalid => {}
                        _ => self.problem(item.location, MERGE_MESSAGE),
                    }
                }
            }
            NodeKind::Invalid => {}
            _ => self.problem(value.location, MERGE_MESSAGE),
        }
        let mut seen: HashSet<String> = own.iter().map(|e| e.key.clone()).collect();
        let mut merged = Vec::new();
        for entry in sources.into_iter().flatten() {
            if seen.insert(entry.key.clone()) {
                merged.push(entry);
            }
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeKind;
    use crate::{ConfigError, Location, MAX_VALUES};

    fn complete_example(lang: &str) -> &'static str {
        const DOC: &str = include_str!("../../../docs/config-schema.md");
        let start = DOC.find("## Complete example").expect("example heading");
        let fence = format!("```{lang}\n");
        let rest = &DOC[start..];
        let open = rest.find(&fence).expect("example fence") + fence.len();
        let body = &rest[open..];
        &body[..body.find("```").expect("closing fence")]
    }

    fn read_all(text: &str) -> (Option<Node>, Vec<ConfigError>) {
        let mut errors = Errors::new();
        let node = read(text, &mut errors);
        (node, errors.into_sorted())
    }

    fn read_ok(text: &str) -> Node {
        let (node, errors) = read_all(text);
        assert!(errors.is_empty(), "{text:?}: {errors:?}");
        node.expect("a tree")
    }

    fn at(line: u32, column: u32) -> Location {
        Location { line, column }
    }

    fn entries(node: &Node) -> &[crate::node::Entry] {
        let NodeKind::Map(entries) = &node.kind else {
            panic!("not a map")
        };
        entries
    }

    #[test]
    fn reads_values_keys_aliases_and_document_markers() {
        for (text, want) in [
            (
                "a:\nb: ~\nc: null\nd: true\ne: 2\nf: -3.5\ng: yes\nh: NO\ni: \"010\"\nj: '0x10'\n",
                r#"{a:null,b:null,c:null,d:true,e:2,f:-3.5,g:"yes",h:"NO",i:"010",j:"0x10"}"#,
            ),
            (
                "k: |\n  line\nl: >-\n  fold\n  ed\nm: \"\\t\"\n",
                r#"{k:"line\n",l:"fold ed",m:"\t"}"#,
            ),
            (
                "1: a\ntrue: b\n~: c\nnull: d\n010: e\n: f\n",
                r#"{1:"a",true:"b",~:"c",null:"d",010:"e",:"f"}"#,
            ),
            (
                "a: &x {b: 1, c: [2]}\nd: *x\ne: &y 5\nf: *y\n",
                "{a:{b:1,c:[2]},d:{b:1,c:[2]},e:5,f:5}",
            ),
            ("a: &x 1\nb: &x 2\nc: *x\n", "{a:1,b:2,c:2}"),
            ("&k a: 1\nb: *k\n", r#"{a:1,b:"a"}"#),
            ("---\na: 1\n", "{a:1}"),
            ("a: 1\n...\n", "{a:1}"),
            ("---\na: 1\n...\n", "{a:1}"),
            ("%YAML 1.1\n---\na: 1\n", "{a:1}"),
            ("%YAML 9.9\n---\na: 1\n", "{a:1}"),
            ("---\n", "null"),
            ("- a: 1\n  b: 2\n- c: 3\n", "[{a:1,b:2},{c:3}]"),
            ("- - - x\n", r#"[[["x"]]]"#),
        ] {
            assert_eq!(read_ok(text).shape(), want, "{text:?}");
        }
    }

    #[test]
    fn merge_keys_follow_every_rule() {
        for (text, want) in [
            // Entries land where << is written, in source order.
            (
                "base: &b {x: 1, y: 2}\nm:\n  a: 0\n  <<: *b\n  z: 3\n",
                "{base:{x:1,y:2},m:{a:0,x:1,y:2,z:3}}",
            ),
            // A key written in the map wins, wherever it sits, with no error.
            (
                "base: &b {x: 1, y: 2}\nm:\n  y: 9\n  <<: *b\nn:\n  <<: *b\n  y: 8\n",
                "{base:{x:1,y:2},m:{y:9,x:1},n:{x:1,y:8}}",
            ),
            // In a list of maps, an earlier map wins, with no error.
            (
                "a: &a {k: 1, p: 1}\nb: &b {k: 2, q: 2}\nm:\n  <<: [*a, *b]\n",
                "{a:{k:1,p:1},b:{k:2,q:2},m:{k:1,p:1,q:2}}",
            ),
            // An inline map works, and so does an empty one.
            ("m: {<<: {a: 1}, b: 2}\n", "{m:{a:1,b:2}}"),
            ("m: {<<: {}}\n", "{m:{}}"),
            // A quoted << is an ordinary key.
            ("m: {\"<<\": 1}\n", "{m:{<<:1}}"),
            ("m: {'<<': 1}\n", "{m:{<<:1}}"),
            // Sources have already resolved their own merges.
            (
                "a: &a {k: 1}\nb: &b {<<: *a, l: 2}\nc: {<<: *b}\n",
                "{a:{k:1},b:{k:1,l:2},c:{k:1,l:2}}",
            ),
        ] {
            assert_eq!(read_ok(text).shape(), want, "{text:?}");
        }
    }

    #[test]
    fn recoverable_problems_are_recorded_and_reading_continues() {
        let tags = "tags aren't supported";
        let keys = "keys must be strings";
        let merge = "needs a map or a list of maps";
        for (text, want, problems) in [
            (
                "a: 010\nb: 2\nc: .inf\n",
                "{a:!,b:2,c:!}",
                vec![(1, 4, "looks like a number"), (3, 4, "looks like a number")],
            ),
            (
                "a: 18446744073709551616\nb: 1e999\n",
                "{a:!,b:!}",
                vec![(1, 4, "outside the supported range"), (2, 4, "too large")],
            ),
            (
                "a: 1\n\"a\": 2\nb: 3\n",
                "{a:1,b:3}",
                vec![(2, 1, "duplicate key `a`, first used on line 1")],
            ),
            (
                "m: {a: 1, a: 2, <<: {b: 3}, c: 4}\n",
                "{m:{a:1,b:3,c:4}}",
                vec![(1, 11, "duplicate key `a`, first used on line 1")],
            ),
            (
                "a: !!str 5\nb: !custom x\nc: !!map {d: 1}\ne: !!seq [1]\n!!str f: 2\n",
                "{a:!,b:!,c:!,e:!,f:2}",
                vec![
                    (1, 4, tags),
                    (2, 4, tags),
                    (3, 4, tags),
                    (4, 4, tags),
                    (5, 1, tags),
                ],
            ),
            (
                "? [a, b]\n: c\n? {d: 1}\n: e\nf: 1\n",
                "{f:1}",
                vec![(1, 3, keys), (3, 3, keys)],
            ),
            (
                "a: &x k\n*x : v\nb: 2\n",
                r#"{a:"k",b:2}"#,
                vec![(2, 1, keys)],
            ),
            ("a: &x [1, *x]\n", "{a:[1,!]}", vec![(1, 11, "contains it")]),
            ("m:\n  <<: 5\n  a: 1\n", "{m:{a:1}}", vec![(2, 7, merge)]),
            (
                "x: &x {k: 1}\nm:\n  <<: [*x, 5]\n",
                "{x:{k:1},m:{k:1}}",
                vec![(3, 12, merge)],
            ),
            (
                "x: &x {a: 1}\nm:\n  <<: *x\n  <<: *x\n",
                "{x:{a:1},m:{a:1}}",
                vec![(4, 3, "duplicate key `<<`, first used on line 3")],
            ),
            ("m:\n  <<: !!map {a: 1}\n", "{m:{}}", vec![(2, 7, tags)]),
        ] {
            let (node, errors) = read_all(text);
            assert_eq!(node.expect(text).shape(), want, "{text:?}");
            assert_eq!(errors.len(), problems.len(), "{text:?}: {errors:?}");
            for (error, (line, column, fragment)) in errors.iter().zip(problems) {
                assert_eq!(error.location, Some(Location { line, column }), "{text:?}");
                assert!(error.message.contains(fragment), "{text:?}: {error:?}");
            }
        }
    }

    #[test]
    fn fatal_problems_stop_with_a_location() {
        for (text, line, column) in [
            ("a: 1\n---\nb: 2\n", 2, 1),
            ("a: 1\n...\n---\nb: 2\n", 3, 1),
            ("a: *nope\n", 1, 4),
            ("a: [1\n", 2, 1),
            ("a: \"x\\q\"\n", 1, 4),
            ("a:\n\tb: 1\n", 2, 2),
            ("a:\n    b: 1\n  c: 2\n", 3, 3),
            ("headers: { A: ${token} }\n", 1, 16),
        ] {
            let (node, errors) = read_all(text);
            assert!(node.is_none(), "{text:?}");
            assert_eq!(
                errors.last().unwrap().location,
                Some(Location { line, column }),
                "{text:?}"
            );
        }
        let (_, errors) = read_all("a: 1\n---\nb: 2\n");
        assert!(errors[0].message.contains("only one YAML document"));
    }

    #[test]
    fn empty_files_say_so() {
        for text in ["", "# only a comment\n", "  \n", "\u{feff}", "...\n"] {
            let (node, errors) = read_all(text);
            assert!(node.is_none(), "{text:?}");
            assert!(errors[0].message.contains("the file is empty"), "{text:?}");
            assert_eq!(errors[0].location, None, "{text:?}");
        }
    }

    #[test]
    fn locations_count_lines_and_characters() {
        let node = read_ok("é: ñandú\nkey: \"日本語\"\nb:\n  - 1\n");
        assert_eq!(node.location, at(1, 1));
        let map = entries(&node);
        assert_eq!(map[0].key_location, at(1, 1));
        assert_eq!(map[0].value.location, at(1, 4));
        assert_eq!(map[1].key_location, at(2, 1));
        assert_eq!(map[1].value.location, at(2, 6));
        assert_eq!(map[2].key_location, at(3, 1));
        assert_eq!(map[2].value.location, at(4, 3));
        let NodeKind::List(items) = &map[2].value.kind else {
            panic!("not a list")
        };
        assert_eq!(items[0].location, at(4, 5));
    }

    #[test]
    fn line_endings_and_a_byte_order_mark_match_plain_files() {
        for text in ["a: 1\r\nb: 2\r\n", "a: 1\rb: 2\r", "a: 1\nb: 2\n"] {
            let node = read_ok(text);
            let map = entries(&node);
            assert_eq!(map[1].key_location, at(2, 1), "{text:?}");
            assert_eq!(map[1].value.location, at(2, 4), "{text:?}");
        }
        let node = read_ok("\u{feff}a: 1\n");
        assert_eq!(entries(&node)[0].key_location, at(1, 1));
    }

    #[test]
    fn aliased_copies_keep_their_definition_location() {
        let node = read_ok("a: &x {b: 1}\nd: *x\n");
        let map = entries(&node);
        assert_eq!(map[1].value.location, at(1, 4));
        assert_eq!(entries(&map[1].value)[0].key_location, at(1, 8));
    }

    #[test]
    fn nesting_limit_is_128_levels() {
        let flow = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        let block = |n: usize| format!("{}x\n", "- ".repeat(n));
        read_ok(&flow(128));
        read_ok(&block(128));
        for text in [flow(129), block(129)] {
            let (node, errors) = read_all(&text);
            assert!(node.is_none());
            assert!(errors[0].message.contains("nesting") && errors[0].message.contains("128"));
        }
    }

    #[test]
    fn hostile_nesting_fails_cleanly() {
        for text in ["[".repeat(5000), format!("{}x\n", "- ".repeat(500_000))] {
            let (node, errors) = read_all(&text);
            assert!(node.is_none());
            assert_eq!(errors.len(), 1);
        }
    }

    #[test]
    fn an_alias_counts_toward_the_nesting_limit() {
        // Root level 1. The anchored value has height 100. A copy inside k
        // lists reaches level k + 101.
        let doc = |k: usize| {
            format!(
                "a: &x {}{}\nb: {}*x{}\n",
                "[".repeat(100),
                "]".repeat(100),
                "[".repeat(k),
                "]".repeat(k),
            )
        };
        read_ok(&doc(27));
        let (node, errors) = read_all(&doc(28));
        assert!(node.is_none());
        assert!(errors[0].message.contains("nesting"));
    }

    #[test]
    fn value_limit_counts_every_value() {
        let list = |items: usize| format!("[{}0]\n", "0,".repeat(items - 1));
        read_ok(&list(MAX_VALUES - 1));
        let (node, errors) = read_all(&list(MAX_VALUES));
        assert!(node.is_none());
        assert!(errors[0].message.contains("250000 values"));
    }

    #[test]
    fn an_alias_bomb_hits_the_value_limit() {
        let mut text = String::from("a0: &a0 [x,x,x,x,x,x,x,x,x,x]\n");
        for level in 1..9 {
            let refs = vec![format!("*a{}", level - 1); 10].join(",");
            text += &format!("a{level}: &a{level} [{refs}]\n");
        }
        let (node, errors) = read_all(&text);
        assert!(node.is_none());
        assert!(errors[0].message.contains("250000 values"));
    }

    #[test]
    fn aliased_text_counts_toward_the_text_limit() {
        let refs = vec!["*s"; 70].join(", ");
        let text = format!("a: &s \"{}\"\nb: [{refs}]\n", "x".repeat(1024 * 1024));
        let (node, errors) = read_all(&text);
        assert!(node.is_none());
        assert!(errors[0].message.contains("64 MiB of text"));
    }

    #[test]
    fn anchors_nested_at_every_level_are_charged() {
        let mut text = format!("[{}0]", "0,".repeat(4999));
        for level in 0..100 {
            text = format!("&a{level} [{text}]");
        }
        let (node, errors) = read_all(&format!("a: {text}\n"));
        assert!(node.is_none());
        assert!(errors[0].message.contains("250000 values"));
    }

    #[test]
    fn reads_the_same_tree_as_the_json_reader() {
        for text in [
            complete_example("json"),
            r#"{"a": "x\/y\té", "b": [1, 2.5, true, null], "c": {}}"#,
        ] {
            let via_yaml = read_ok(text);
            let mut errors = Errors::new();
            let via_json = crate::json::read(text, &mut errors).expect("valid JSON");
            assert!(errors.is_empty());
            assert!(via_yaml == via_json, "the trees differ, locations included");
        }
    }

    #[test]
    fn reads_the_yaml_example() {
        read_ok(complete_example("yaml"));
    }
}
