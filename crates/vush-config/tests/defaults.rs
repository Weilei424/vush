//! The defaults and limits in `docs/config-schema.md` are the ones the
//! compiler uses.
//!
//! Every default the doc states, in a table or in a sentence, is pinned here
//! with its check, and the pinned list must equal what the doc says. A changed
//! or new default therefore fails until this file is updated too. Each check
//! builds whole plans with `vush_config::parse`.

use std::collections::BTreeSet;
use std::fmt::Debug;
use std::time::Duration;

use vush_config::{Format, MAX_DEPTH, MAX_FILE_BYTES, MAX_TEXT_BYTES, MAX_VALUES, parse};
use vush_core::plan::{
    Expect, HttpSettings, MAX_DURATION, RequestStep, ResourceLimits, StatusMatch, Step,
    WorkloadPlan, parse_duration,
};

use Check::{Holds, Same};

const DOC: &str = include_str!("../../../docs/config-schema.md");

// The doc reader.

/// A Markdown table, under the nearest heading above it.
#[derive(Debug)]
struct Table {
    heading: String,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// What a line of the doc is.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// A heading, a fence, or a line inside a fenced code block.
    Other,
    /// A table line: it starts with `|`.
    Table,
    /// Any other line.
    Prose,
}

/// One line of the doc, with the nearest heading above it.
struct Line<'a> {
    heading: &'a str,
    kind: Kind,
    text: &'a str,
}

/// Every line of `doc`. A line starting with ```` ``` ```` opens or closes a
/// fenced code block. A line starting with `#` outside one is a heading, named
/// by its text after the `#` marks and one space.
fn lines(doc: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut heading = "";
    let mut in_fence = false;
    for text in doc.lines() {
        let kind = if text.starts_with("```") {
            in_fence = !in_fence;
            Kind::Other
        } else if in_fence {
            Kind::Other
        } else if text.starts_with('#') {
            let name = text.trim_start_matches('#');
            heading = name.strip_prefix(' ').unwrap_or(name);
            Kind::Other
        } else if text.starts_with('|') {
            Kind::Table
        } else {
            Kind::Prose
        };
        out.push(Line {
            heading,
            kind,
            text,
        });
    }
    out
}

/// The cells of a table line: trimmed, without the outer `|`s.
fn cells(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = line.strip_suffix('|').unwrap_or(line);
    line.split('|').map(|cell| cell.trim().to_owned()).collect()
}

/// Every table outside fenced code blocks: a run of lines starting with `|`.
/// The first line is the header, the second the `|---|` separator, and the
/// rest are rows. A line whose cell count differs from the header's is an
/// error, never a skipped row.
fn tables(doc: &str) -> Result<Vec<Table>, String> {
    lines(doc)
        .chunk_by(|a, b| a.kind == Kind::Table && b.kind == Kind::Table)
        .filter(|run| run[0].kind == Kind::Table)
        .map(table)
        .collect()
}

fn table(run: &[Line<'_>]) -> Result<Table, String> {
    let header = cells(run[0].text);
    let mut rows = Vec::new();
    for line in &run[1..] {
        let row = cells(line.text);
        if row.len() != header.len() {
            return Err(format!(
                "a table line with {} cells under a header with {}: {}",
                row.len(),
                header.len(),
                line.text
            ));
        }
        rows.push(row);
    }
    let separator = rows.first().is_some_and(|row| {
        row.iter()
            .all(|cell| !cell.is_empty() && cell.chars().all(|c| matches!(c, '-' | ':')))
    });
    if !separator {
        return Err(format!(
            "a table header without a `|---|` line below it: {}",
            run[0].text
        ));
    }
    rows.remove(0);
    Ok(Table {
        heading: run[0].heading.to_owned(),
        header,
        rows,
    })
}

/// The doc's tables. Panics if one can't be read.
fn doc_tables() -> Vec<Table> {
    tables(DOC).unwrap_or_else(|error| panic!("cannot read the doc's tables: {error}"))
}

/// Every ``default `…` `` phrase outside tables and fenced code blocks, with
/// the nearest heading above it. A phrase without a closing backtick runs to
/// the end of its line.
fn prose_defaults(doc: &str) -> BTreeSet<(String, String)> {
    const OPEN: &str = "default `";
    let mut found = BTreeSet::new();
    for line in lines(doc).iter().filter(|line| line.kind == Kind::Prose) {
        let mut rest = line.text;
        while let Some(start) = rest.find(OPEN) {
            let value = start + OPEN.len();
            let end = rest[value..]
                .find('`')
                .map_or(rest.len(), |close| value + close + 1);
            found.insert((line.heading.to_owned(), rest[start..end].to_owned()));
            rest = &rest[end..];
        }
    }
    found
}

#[test]
fn table_reader_rejects_a_row_with_the_wrong_cell_count() {
    assert!(tables("## H\n\n| a | b |\n|---|---|\n| 1 |\n").is_err());
}

#[test]
fn table_reader_reads_headings_headers_and_rows() {
    let tables =
        tables("## `x`\n\n| Key | Default |\n|---|---|\n| `a` | `1` |\n| `b` | none |\n").unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].heading, "`x`");
    assert_eq!(tables[0].header, ["Key", "Default"]);
    assert_eq!(tables[0].rows, [["`a`", "`1`"], ["`b`", "none"]]);
}

// Building files.

/// A minimal valid file. The `#` lines are markers that edits replace.
const BASE: &str = "\
schema_version: 1
name: defaults
#FILE
scenarios:
  - name: s
    load: { model: closed_vus, vus: 1, iterations: 1 }
    #SCENARIO
    steps:
      - request:
          method: GET
          url: \"http://localhost/\"
          #REQUEST
      #STEPS
";
const LOAD: &str = "load: { model: closed_vus, vus: 1, iterations: 1 }";

/// Text replacements, each `(old, new)`. `old` occurs exactly once in
/// [`BASE`]: a marker, or a literal such as [`LOAD`].
type Edits = &'static [(&'static str, &'static str)];

/// Builds [`BASE`] with `edits` applied in order. Every `$` in a replacement
/// becomes `value`. A replacement's first line takes the old text's place,
/// and each later line gets the leading spaces of the line that held it.
/// Panics if an old text doesn't occur exactly once, or if the file doesn't
/// build.
fn build(edits: &[(&str, &str)], value: &str) -> WorkloadPlan {
    let mut text = BASE.to_owned();
    for &(old, new) in edits {
        let count = text.matches(old).count();
        assert_eq!(count, 1, "`{old}` must occur exactly once in:\n{text}");
        let at = text.find(old).expect("counted above");
        let line = &text[text[..at].rfind('\n').map_or(0, |newline| newline + 1)..];
        let indent = line.len() - line.trim_start_matches(' ').len();
        let new = new
            .replace('$', value)
            .replace('\n', &format!("\n{}", " ".repeat(indent)));
        text.replace_range(at..at + old.len(), &new);
    }
    parse(&text, Format::Yaml).unwrap_or_else(|errors| {
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        panic!(
            "this file doesn't build:\n{text}\nerrors:\n{}",
            errors.join("\n")
        )
    })
}

/// [`BASE`] with every optional key left out.
fn minimal() -> WorkloadPlan {
    build(&[], "")
}

fn request(step: &Step) -> &RequestStep {
    match step {
        Step::Request(request) => request,
        other => panic!("expected a request step, found {other:?}"),
    }
}

/// The scenario's first step, the request in [`BASE`].
fn first_request(plan: &WorkloadPlan) -> &RequestStep {
    request(&plan.scenarios()[0].steps[0])
}

#[track_caller]
fn assert_empty<T: Debug>(list: &[T]) {
    assert!(list.is_empty(), "expected an empty list, found {list:?}");
}

// The pinned defaults.

/// How a pinned default is checked.
enum Check {
    /// For each pair, the file built with the second edits (`$` = the
    /// documented value) equals the one built with the first.
    Same(&'static [(Edits, Edits)]),
    /// Any other check.
    Holds(fn()),
}

/// A default as the doc states it, with its check.
struct Known {
    /// The nearest heading above it.
    heading: &'static str,
    /// The key cell, or for a sentence, the key it sets.
    key: &'static str,
    /// The Default cell, or for a sentence, the text after `default `. The
    /// documented value is this with its backticks removed.
    default: &'static str,
    check: Check,
}

/// Every row with a Default cell in the doc's tables, exactly as written.
const TABLE_DEFAULTS: &[Known] = &[
    Known {
        heading: "Top level",
        key: "`base_url`",
        default: "none",
        check: Holds(|| assert_eq!(minimal().base_url(), None)),
    },
    Known {
        heading: "Top level",
        key: "`http`",
        default: "the defaults in that table",
        check: Holds(|| {
            let plan = minimal();
            assert_eq!(plan.http(), &HttpSettings::default());
            assert_eq!(&plan.scenarios()[0].http, plan.http());
            assert_eq!(build(&[("#FILE", "http: {}")], ""), plan);
        }),
    },
    Known {
        heading: "Top level",
        key: "`limits`",
        default: "the defaults in that table",
        check: Holds(|| {
            let plan = minimal();
            assert_eq!(plan.limits(), &ResourceLimits::default());
            assert_eq!(&plan.scenarios()[0].limits, plan.limits());
            assert_eq!(build(&[("#FILE", "limits: {}")], ""), plan);
        }),
    },
    Known {
        heading: "Top level",
        key: "`setup`, `teardown`",
        default: "empty",
        check: Holds(|| {
            let plan = minimal();
            assert_empty(plan.setup());
            assert_empty(plan.teardown());
        }),
    },
    Known {
        heading: "Top level",
        key: "`thresholds`",
        default: "empty",
        check: Holds(|| assert_empty(minimal().thresholds())),
    },
    Known {
        heading: "`http`",
        key: "`version`",
        default: "`auto`",
        check: Same(&[(&[], &[("#FILE", "http: { version: $ }")])]),
    },
    Known {
        heading: "`http`",
        key: "`pool`",
        default: "`shared`",
        check: Same(&[(&[], &[("#FILE", "http: { pool: $ }")])]),
    },
    Known {
        heading: "`http`",
        key: "`timeout`",
        default: "`30s`",
        check: Same(&[(&[], &[("#FILE", "http: { timeout: $ }")])]),
    },
    Known {
        heading: "`http`",
        key: "`redirects`",
        default: "`5`",
        check: Same(&[(&[], &[("#FILE", "http: { redirects: $ }")])]),
    },
    Known {
        heading: "`http`",
        key: "`cookies`",
        default: "`true`",
        check: Same(&[(&[], &[("#FILE", "http: { cookies: $ }")])]),
    },
    Known {
        heading: "`http`",
        key: "`max_response_size`",
        default: "`1MiB`",
        check: Same(&[(&[], &[("#FILE", "http: { max_response_size: $ }")])]),
    },
    Known {
        heading: "`http`",
        key: "`tls.ca_file`",
        default: "none",
        check: Holds(|| {
            let plan = minimal();
            assert_eq!(plan.http().tls.ca_file, None);
            assert_eq!(plan.scenarios()[0].http.tls.ca_file, None);
        }),
    },
    Known {
        heading: "`http`",
        key: "`tls.insecure`",
        default: "`false`",
        check: Same(&[(&[], &[("#FILE", "http: { tls: { insecure: $ } }")])]),
    },
    Known {
        heading: "`limits`",
        key: "`max_request_rate`",
        default: "none",
        check: Holds(|| {
            let plan = minimal();
            assert_eq!(plan.max_request_rate(), None);
            assert_eq!(plan.scenarios()[0].max_request_rate, None);
        }),
    },
    Known {
        heading: "`limits`",
        key: "`max_variables`",
        default: "`256`",
        check: Same(&[(&[], &[("#FILE", "limits: { max_variables: $ }")])]),
    },
    Known {
        heading: "`limits`",
        key: "`max_variable_size`",
        default: "`64KiB`",
        check: Same(&[(&[], &[("#FILE", "limits: { max_variable_size: $ }")])]),
    },
    Known {
        heading: "`limits`",
        key: "`max_cookies`",
        default: "`64`",
        check: Same(&[(&[], &[("#FILE", "limits: { max_cookies: $ }")])]),
    },
    Known {
        heading: "`limits`",
        key: "`hook_timeout`",
        default: "`60s`",
        check: Same(&[(&[], &[("#FILE", "limits: { hook_timeout: $ }")])]),
    },
    Known {
        heading: "`scenarios`",
        key: "`http`",
        default: "inherits the file-level settings",
        check: Holds(|| {
            let plan = build(&[("#FILE", "http: { timeout: 7s }")], "");
            assert_eq!(&plan.scenarios()[0].http, plan.http());
            assert_eq!(plan.scenarios()[0].http.timeout, Duration::from_secs(7));
            assert_eq!(build(&[("#SCENARIO", "http: {}")], ""), minimal());
        }),
    },
    Known {
        heading: "`scenarios`",
        key: "`limits`",
        default: "inherits the file-level limits",
        check: Holds(|| {
            let plan = build(&[("#FILE", "limits: { max_variables: 7 }")], "");
            assert_eq!(&plan.scenarios()[0].limits, plan.limits());
            assert_eq!(plan.scenarios()[0].limits.max_variables, 7);
            assert_eq!(build(&[("#SCENARIO", "limits: {}")], ""), minimal());
        }),
    },
    Known {
        heading: "`scenarios`",
        key: "`state`",
        default: "`persist`",
        check: Same(&[(&[], &[("#SCENARIO", "state: $")])]),
    },
    Known {
        heading: "`scenarios`",
        key: "`setup`, `teardown`",
        default: "empty",
        check: Holds(|| {
            let plan = minimal();
            assert_empty(&plan.scenarios()[0].setup);
            assert_empty(&plan.scenarios()[0].teardown);
        }),
    },
    Known {
        heading: "`scenarios`",
        key: "`vu_setup`, `vu_teardown`",
        default: "empty",
        check: Holds(|| {
            let plan = minimal();
            assert_empty(&plan.scenarios()[0].vu_setup);
            assert_empty(&plan.scenarios()[0].vu_teardown);
        }),
    },
    Known {
        heading: "`request`",
        key: "`name`",
        default: "none",
        check: Holds(|| assert_eq!(first_request(&minimal()).name, None)),
    },
    Known {
        heading: "`request`",
        key: "`query`, `headers`",
        default: "empty",
        check: Holds(|| {
            let plan = minimal();
            assert_empty(&first_request(&plan).query);
            assert_empty(&first_request(&plan).headers);
        }),
    },
    Known {
        heading: "`request`",
        key: "`json`, `body` or `form`",
        default: "none",
        check: Holds(|| assert_eq!(first_request(&minimal()).body, None)),
    },
    Known {
        heading: "`request`",
        key: "`timeout`",
        default: "the step's `http.timeout`",
        check: Holds(|| {
            let timeout = |edits: &[(&str, &str)]| first_request(&build(edits, "")).timeout;
            let seconds = Duration::from_secs;
            assert_eq!(
                timeout(&[("#SCENARIO", "http: { timeout: 7s }")]),
                seconds(7)
            );
            assert_eq!(timeout(&[("#FILE", "http: { timeout: 9s }")]), seconds(9));
            let plan = build(
                &[
                    (
                        "#FILE",
                        "http: { timeout: 9s }\n\
                         setup:\n  - request: { method: GET, url: \"http://localhost/\" }",
                    ),
                    ("#SCENARIO", "http: { timeout: 7s }"),
                ],
                "",
            );
            assert_eq!(request(&plan.setup()[0]).timeout, seconds(9));
            assert_eq!(first_request(&plan).timeout, seconds(7));
        }),
    },
    Known {
        heading: "`request`",
        key: "`required`",
        default: "`true`",
        check: Same(&[(&[], &[("#REQUEST", "required: $")])]),
    },
    Known {
        heading: "`request`",
        key: "`retry`",
        default: "no retries",
        check: Holds(|| assert_eq!(first_request(&minimal()).retry, None)),
    },
    Known {
        heading: "`request`",
        key: "`expect`",
        default: "see below",
        check: Holds(|| {
            let plan = minimal();
            let expect = &first_request(&plan).expect;
            assert_eq!(expect, &Expect::default());
            assert_eq!(expect.status, StatusMatch::AnySuccess);
            assert_eq!(build(&[("#REQUEST", "expect: {}")], ""), plan);
        }),
    },
    Known {
        heading: "`request`",
        key: "`extract`",
        default: "empty",
        check: Holds(|| assert_empty(&first_request(&minimal()).extract)),
    },
    Known {
        heading: "`retry`",
        key: "`backoff`",
        default: "`100ms`",
        check: Same(&[(
            &[("#REQUEST", "retry: { attempts: 2 }")],
            &[("#REQUEST", "retry: { attempts: 2, backoff: $ }")],
        )]),
    },
    Known {
        heading: "`retry`",
        key: "`allow_non_idempotent`",
        default: "`false`",
        check: Same(&[(
            &[("#REQUEST", "retry: { attempts: 2 }")],
            &[(
                "#REQUEST",
                "retry: { attempts: 2, allow_non_idempotent: $ }",
            )],
        )]),
    },
    Known {
        heading: "`repeat`",
        key: "`deadline`",
        default: "`60s`",
        check: Same(&[(
            &[("#STEPS", "- repeat: { count: 1, steps: [ { wait: 0s } ] }")],
            &[(
                "#STEPS",
                "- repeat: { count: 1, deadline: $, steps: [ { wait: 0s } ] }",
            )],
        )]),
    },
];

/// Every default the doc states in a sentence, as ``default `…` ``.
const PROSE_DEFAULTS: &[Known] = &[
    Known {
        heading: "`closed_vus`",
        key: "`start`",
        default: "`0`",
        check: Same(&[(
            &[(
                LOAD,
                "load: { model: closed_vus, stages: [ { to: 1, over: 1s } ] }",
            )],
            &[(
                LOAD,
                "load: { model: closed_vus, start: $, stages: [ { to: 1, over: 1s } ] }",
            )],
        )]),
    },
    Known {
        heading: "`iteration_rate`",
        key: "`queue.capacity`",
        default: "`1000`",
        check: Same(&[
            (
                &[(
                    LOAD,
                    "load: { model: iteration_rate, rate: 1/s, duration: 1s, max_vus: 1 }",
                )],
                &[(
                    LOAD,
                    "load: { model: iteration_rate, rate: 1/s, duration: 1s, max_vus: 1, \
                     queue: { capacity: $ } }",
                )],
            ),
            (
                &[(
                    LOAD,
                    "load: { model: iteration_rate, rate: 1/s, duration: 1s, max_vus: 1 }",
                )],
                &[(
                    LOAD,
                    "load: { model: iteration_rate, rate: 1/s, duration: 1s, max_vus: 1, \
                     queue: {} }",
                )],
            ),
        ]),
    },
    Known {
        heading: "`iteration_rate`",
        key: "`queue.max_wait`",
        default: "`1s`",
        check: Same(&[(
            &[(
                LOAD,
                "load: { model: iteration_rate, rate: 1/s, duration: 1s, max_vus: 1 }",
            )],
            &[(
                LOAD,
                "load: { model: iteration_rate, rate: 1/s, duration: 1s, max_vus: 1, \
                 queue: { max_wait: $ } }",
            )],
        )]),
    },
    Known {
        heading: "`iteration_rate`",
        key: "`start`",
        default: "`0/s`",
        check: Same(&[
            (
                &[(
                    LOAD,
                    "load: { model: iteration_rate, max_vus: 1, \
                     stages: [ { to: 1/s, over: 1s } ] }",
                )],
                &[(
                    LOAD,
                    "load: { model: iteration_rate, max_vus: 1, start: $, \
                     stages: [ { to: 1/s, over: 1s } ] }",
                )],
            ),
            // `request_rate`'s `start` is "the same as for `iteration_rate`".
            (
                &[(
                    LOAD,
                    "load: { model: request_rate, vus: 1, stages: [ { to: 1/s, over: 1s } ] }",
                )],
                &[(
                    LOAD,
                    "load: { model: request_rate, vus: 1, start: $, \
                     stages: [ { to: 1/s, over: 1s } ] }",
                )],
            ),
        ]),
    },
];

/// Panics unless the defaults `found` in the doc are the `pinned` ones,
/// listing every pinned row the doc lacks and every doc row not pinned here.
fn assert_same_set<T: Ord + Debug>(found: &BTreeSet<T>, pinned: &BTreeSet<T>) {
    let missing: Vec<&T> = pinned.difference(found).collect();
    let unexpected: Vec<&T> = found.difference(pinned).collect();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "the doc's defaults differ from the ones pinned in this file\n\
         missing (pinned here, not in the doc): {missing:#?}\n\
         unexpected (in the doc, not pinned here): {unexpected:#?}\n\
         every documented default needs a pinned row, with its check, in this file"
    );
}

#[test]
fn doc_defaults_are_the_known_ones() {
    let mut found = BTreeSet::new();
    for table in doc_tables() {
        let Some(column) = table.header.iter().position(|cell| cell == "Default") else {
            continue;
        };
        for row in &table.rows {
            if !row[column].is_empty() {
                found.insert((table.heading.clone(), row[0].clone(), row[column].clone()));
            }
        }
    }
    let pinned = TABLE_DEFAULTS
        .iter()
        .map(|known| {
            let text = |s: &str| s.to_owned();
            (text(known.heading), text(known.key), text(known.default))
        })
        .collect();
    assert_same_set(&found, &pinned);
}

#[test]
fn doc_prose_defaults_are_the_known_ones() {
    let pinned = PROSE_DEFAULTS
        .iter()
        .map(|known| {
            (
                known.heading.to_owned(),
                format!("default {}", known.default),
            )
        })
        .collect();
    assert_same_set(&prose_defaults(DOC), &pinned);
}

#[test]
fn doc_keeps_the_defaults_stated_in_sentences() {
    for sentence in [
        "Ramping: `stages` and an optional `start`, the same as for `iteration_rate`.",
        "If absent, any 2xx or 3xx status passes",
    ] {
        assert!(DOC.contains(sentence), "the doc no longer says: {sentence}");
    }
}

#[test]
fn every_documented_default_holds() {
    for known in TABLE_DEFAULTS.iter().chain(PROSE_DEFAULTS) {
        eprintln!(
            "checking {} under {}: {}",
            known.key, known.heading, known.default
        );
        match known.check {
            Same(pairs) => {
                let value = known.default.replace('`', "");
                for &(without, with) in pairs {
                    assert_eq!(
                        build(with, &value),
                        build(without, &value),
                        "the file with {with:?} (left) must build the same plan as the file \
                         with {without:?} (right), where `$` is `{value}`"
                    );
                }
            }
            Holds(check) => check(),
        }
    }
}

// Limits.

/// The `<n>` of text that begins `At most <n>`, without its thousands commas,
/// and multiplied by 1024 × 1024 when ` MiB` follows it.
fn at_most(text: &str) -> Option<usize> {
    let rest = text.strip_prefix("At most ")?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != ',')
        .unwrap_or(rest.len());
    let number = rest[..end].trim_end_matches(',');
    let n: usize = number.replace(',', "").parse().ok()?;
    if rest[number.len()..].starts_with(" MiB") {
        n.checked_mul(1024 * 1024)
    } else {
        Some(n)
    }
}

#[test]
fn file_rules_limits_match_the_constants() {
    let tables = doc_tables();
    let rules = tables
        .iter()
        .find(|table| table.heading == "File rules")
        .expect("a table under the `File rules` heading");
    for (rule, constant) in [
        ("File size", MAX_FILE_BYTES),
        ("Nesting", MAX_DEPTH),
        ("Values", MAX_VALUES),
        ("Text", MAX_TEXT_BYTES),
    ] {
        let row = rules
            .rows
            .iter()
            .find(|row| row[0] == rule)
            .unwrap_or_else(|| panic!("no `{rule}` row in the File rules table"));
        let limit = at_most(&row[1]).unwrap_or_else(|| {
            panic!(
                "the `{rule}` rule doesn't begin with `At most <n>`: {}",
                row[1]
            )
        });
        assert_eq!(limit, constant, "the `{rule}` rule's limit");
    }
}

#[test]
fn steps_nesting_limit_matches_max_depth() {
    const LINK: &str = "[nesting limit](#file-rules) of ";
    let (_, after) = DOC
        .split_once(LINK)
        .unwrap_or_else(|| panic!("no line in the doc contains `{LINK}`"));
    let number: String = after.chars().take_while(char::is_ascii_digit).collect();
    assert_eq!(
        number.parse::<usize>().ok(),
        Some(MAX_DEPTH),
        "the nesting limit in the Steps section"
    );
}

#[test]
fn duration_cap_matches_max_duration() {
    let mentions: Vec<&str> = DOC
        .match_indices("`720h`")
        .map(|(at, cap)| &DOC[at + cap.len()..])
        .collect();
    assert!(!mentions.is_empty(), "the doc no longer mentions `720h`");
    for after in mentions {
        assert!(
            after.starts_with(" (30 days)"),
            "every `720h` must read `720h` (30 days), but one reads `720h`{}",
            after.lines().next().unwrap_or("")
        );
    }
    assert_eq!(parse_duration("720h").ok(), Some(MAX_DURATION));
    assert_eq!(MAX_DURATION.as_secs(), 30 * 24 * 60 * 60);
}
