//! Size, nesting, value and error-count limits through the public API.

use vush_config::{
    ConfigError, Format, Location, MAX_DEPTH, MAX_ERRORS, MAX_FILE_BYTES, MAX_VALUES, parse,
};

fn file(steps: &str) -> String {
    format!(
        r#"{{"schema_version": 1, "name": "n", "scenarios": [{{"name": "s", "load": {{"model": "closed_vus", "vus": 1, "duration": "1s"}}, "steps": [{steps}]}}]}}"#
    )
}

fn nested(n: usize, open: &str, close: &str, inner: &str) -> String {
    format!("{}{inner}{}", open.repeat(n), close.repeat(n))
}

fn on_small_stack(test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(test)
        .unwrap()
        .join()
        .unwrap();
}

const BOTH: [Format; 2] = [Format::Yaml, Format::Json];

const WAIT: &str = r#"{"wait": "1s"}"#;

fn repeats(n: usize, leaf: &str) -> String {
    nested(n, r#"{"repeat": {"count": 1, "steps": ["#, "]}}", leaf)
}

fn repeat_steps(n: usize) -> String {
    repeats(n, WAIT)
}

/// `n` nested `if`s whose `when` reads `var`, around `leaf`.
fn ifs(n: usize, var: &str, leaf: &str) -> String {
    let open = format!(r#"{{"if": {{"when": {{"var": "{var}", "exists": true}}, "then": ["#);
    nested(n, &open, "]}}", leaf)
}

fn if_steps(n: usize) -> String {
    ifs(n, "vu.id", WAIT)
}

/// An `if` whose `when` is `n` nested `not`s around `leaf`.
fn nots(n: usize, leaf: &str) -> String {
    let when = nested(n, r#"{"not": "#, "}", leaf);
    format!(r#"{{"if": {{"when": {when}, "then": [{WAIT}]}}}}"#)
}

fn not_steps(n: usize) -> String {
    nots(n, r#"{"var": "vu.id", "exists": true}"#)
}

/// A request whose `json` body is `n` nested lists around `leaf`.
fn json_body(n: usize, leaf: &str) -> String {
    let body = nested(n, "[", "]", leaf);
    format!(r#"{{"request": {{"method": "GET", "url": "http://localhost/", "json": {body}}}}}"#)
}

fn json_steps(n: usize) -> String {
    json_body(n, "1")
}

/// A shape's name, its builder, and the deepest `n` that is allowed.
type Shape = (&'static str, fn(usize) -> String, usize);

fn shapes() -> [Shape; 4] {
    [
        ("repeat", repeat_steps, 41),
        ("if", if_steps, 41),
        ("not", not_steps, 121),
        ("json", json_steps, 122),
    ]
}

#[test]
fn deepest_allowed_nesting_builds_on_a_small_stack() {
    for (name, make, deepest) in shapes() {
        let text = file(&make(deepest));
        on_small_stack(move || {
            let yaml = parse(&text, Format::Yaml).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let json = parse(&text, Format::Json).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!(yaml, json, "{name}");
        });
    }
}

#[test]
fn a_failing_leaf_at_the_deepest_nesting_is_reported_on_a_small_stack() {
    let [repeat, if_, not, json] = shapes().map(|(_, _, deepest)| deepest);
    let zero_timeout =
        r#"{"request": {"method": "GET", "url": "http://localhost/", "timeout": "0s"}}"#;
    // A request takes two map levels where a wait takes one, so the deepest
    // `repeat` around a request is one less than around a wait.
    let too_deep = file(&repeats(repeat, zero_timeout));
    let errors = parse(&too_deep, Format::Json).expect_err("too deep");
    assert!(errors[0].message.contains("nesting"), "{errors:?}");
    let unset = "sets this variable";
    let cases = [
        (
            "repeat",
            repeats(repeat - 1, zero_timeout),
            "longer than zero",
        ),
        // Only the innermost `if` reads the unset name.
        ("if", ifs(if_ - 1, "vu.id", &ifs(1, "nope", WAIT)), unset),
        (
            "not",
            nots(not, r#"{"var": "nope", "exists": true}"#),
            unset,
        ),
        ("json", json_body(json, r#""${nope}""#), unset),
    ];
    for (name, steps, fragment) in cases {
        let text = file(&steps);
        on_small_stack(move || {
            for format in BOTH {
                let errors = parse(&text, format).expect_err(name);
                assert_eq!(errors.len(), 1, "{name} {format:?}: {errors:?}");
                assert!(
                    errors[0].message.contains(fragment),
                    "{name} {format:?}: {errors:?}"
                );
            }
        });
    }
}

#[test]
fn one_level_more_is_refused() {
    for (name, make, deepest) in shapes() {
        let text = file(&make(deepest + 1));
        for format in BOTH {
            let errors = parse(&text, format).expect_err(name);
            assert_eq!(errors.len(), 1, "{name} {format:?}: {errors:?}");
            let message = &errors[0].message;
            assert!(
                message.contains("nesting") && message.contains("128"),
                "{name}: {message}"
            );
        }
    }
}

#[test]
fn text_at_the_size_limit_is_read_and_one_byte_more_is_not() {
    for format in BOTH {
        let mut text = String::from("{}");
        text.push_str(&" ".repeat(MAX_FILE_BYTES - text.len()));
        assert_eq!(text.len(), MAX_FILE_BYTES);
        let errors = parse(&text, format).expect_err("no keys");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("missing required key"));
        assert!(!errors[0].message.contains("larger than"));

        text.push(' ');
        let errors = parse(&text, format).expect_err("too big");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0]
                .message
                .contains("larger than the limit of 10 MiB")
        );
    }
}

fn valid_with_unknown_keys(count: usize, format: Format) -> String {
    match format {
        Format::Json => {
            let extra: String = (0..count).map(|i| format!(r#", "k{i}": 1"#)).collect();
            let base = file(r#"{"wait": "1s"}"#);
            format!("{}{extra}}}", &base[..base.len() - 1])
        }
        Format::Yaml => {
            let extra: String = (0..count).map(|i| format!("k{i}: 1\n")).collect();
            format!(
                "schema_version: 1\nname: n\n{extra}scenarios:\n  - name: s\n    load: {{model: closed_vus, vus: 1, duration: 1s}}\n    steps:\n      - wait: 1s\n"
            )
        }
    }
}

#[test]
fn the_error_list_is_capped() {
    for format in BOTH {
        let errors = parse(&valid_with_unknown_keys(MAX_ERRORS, format), format).expect_err("a");
        assert_eq!(errors.len(), MAX_ERRORS, "{format:?}");
        assert!(
            errors
                .iter()
                .all(|e| !e.message.contains("too many errors"))
        );

        let errors = parse(&valid_with_unknown_keys(150, format), format).expect_err("b");
        assert_eq!(errors.len(), MAX_ERRORS + 1, "{format:?}");
        assert!(errors.last().unwrap().message.contains("too many errors"));
    }
}

#[test]
fn an_alias_bomb_is_refused_through_the_public_api() {
    let mut text = String::from("a0: &a0 [x, x, x, x, x, x, x, x, x, x]\n");
    for level in 1..9 {
        let refs = vec![format!("*a{}", level - 1); 10].join(", ");
        text.push_str(&format!("a{level}: &a{level} [{refs}]\n"));
    }
    let errors = parse(&text, Format::Yaml).expect_err("bomb");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].message.contains("250000 values"));
}

fn count_errors(errors: &[ConfigError]) -> usize {
    errors
        .iter()
        .filter(|e| e.message.contains("250000 values"))
        .count()
}

fn list_of(item: &str, count: usize, format: Format) -> String {
    match format {
        Format::Json => format!("[{}]", vec![item; count].join(",")),
        Format::Yaml => format!("[{}]", vec![item; count].join(", ")),
    }
}

#[test]
fn value_limit_counts_every_kind_of_value() {
    // The list itself is one value, so MAX_VALUES - 1 items fit exactly.
    for item in ["null", r#""""#, "{}"] {
        for format in BOTH {
            let text = list_of(item, MAX_VALUES - 1, format);
            let errors = parse(&text, format).expect_err("not a map");
            assert_eq!(count_errors(&errors), 0, "{item} {format:?}: {errors:?}");

            let text = list_of(item, MAX_VALUES, format);
            let errors = parse(&text, format).expect_err("too many");
            assert_eq!(count_errors(&errors), 1, "{item} {format:?}: {errors:?}");
        }
    }

    let text = list_of("null", MAX_VALUES, Format::Json);
    let errors = parse(&text, Format::Json).expect_err("too many");
    let at = errors
        .iter()
        .find(|e| e.message.contains("250000 values"))
        .and_then(|e| e.location);
    assert_eq!(
        at,
        Some(Location {
            line: 1,
            column: 1_249_997
        })
    );
}

#[test]
fn an_empty_map_at_the_nesting_limit_counts() {
    let make = |levels: usize| nested(levels - 1, r#"{"a":"#, "}", "{}");
    for format in BOTH {
        let errors = parse(&make(MAX_DEPTH), format).expect_err("no plan");
        assert!(
            errors.iter().all(|e| !e.message.contains("nesting")),
            "{format:?}: {errors:?}"
        );

        let errors = parse(&make(MAX_DEPTH + 1), format).expect_err("too deep");
        let deep: Vec<_> = errors
            .iter()
            .filter(|e| e.message.contains("nesting") && e.message.contains("128"))
            .collect();
        assert_eq!(deep.len(), 1, "{format:?}: {errors:?}");
        assert_eq!(errors.len(), 1, "{format:?}: {errors:?}");
    }
}

/// A small YAML file whose aliases copy one list of `refs` unset `"${n}"`
/// references `copies` times, `depth` lists down in a `json` body.
fn aliased_references(depth: usize, refs: usize, copies: usize) -> String {
    let list = format!("[{}]", vec![r#""${n}""#; refs].join(", "));
    let aliases = vec!["*b"; copies - 1].join(", ");
    let body = nested(depth, "[", "]", &format!("[&b {list}, {aliases}]"));
    format!(
        "schema_version: 1\nname: n\nscenarios:\n  - name: s\n    load: {{model: closed_vus, vus: 1, duration: 1s}}\n    steps:\n      - request: {{method: POST, url: \"http://localhost/\", json: {body}}}\n"
    )
}

/// Plan errors are bounded before they are built, not only when they are
/// shown. Without that bound, the alias file below took seconds and hundreds
/// of megabytes even in a release build; this test should take well under a
/// second in a debug build.
#[test]
fn plan_errors_stay_bounded() {
    on_small_stack(|| {
        let text = aliased_references(118, 1000, 240);
        assert!(text.len() < 10 * 1024, "{}", text.len());
        let errors = parse(&text, Format::Yaml).expect_err("aliases");
        assert_eq!(errors.len(), MAX_ERRORS + 1);
        assert!(
            errors
                .iter()
                .take(MAX_ERRORS)
                .all(|e| e.message.contains("sets this variable")),
            "{errors:?}"
        );
        let last = errors.last().map(|e| e.message.as_str());
        assert_eq!(last, Some("too many errors, only the first 100 are shown"));

        let key = "k".repeat(1024 * 1024);
        let refs = vec![r#""${n}""#; 400].join(", ");
        let text = file(&format!(
            r#"{{"request": {{"method": "POST", "url": "http://localhost/", "json": {{"{key}": [{refs}]}}}}}}"#
        ));
        for format in BOTH {
            let errors = parse(&text, format).expect_err("long key");
            assert!(
                errors.len() <= MAX_ERRORS + 1,
                "{format:?}: {}",
                errors.len()
            );
            assert!(
                errors.iter().all(|e| e.path.len() < 1000),
                "{format:?}: {:?}",
                errors.iter().map(|e| e.path.len()).max()
            );
        }
    });
}
