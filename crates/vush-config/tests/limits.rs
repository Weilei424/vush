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

fn repeat_steps(n: usize) -> String {
    nested(
        n,
        r#"{"repeat": {"count": 1, "steps": ["#,
        "]}}",
        r#"{"wait": "1s"}"#,
    )
}

fn if_steps(n: usize) -> String {
    nested(
        n,
        r#"{"if": {"when": {"var": "x", "exists": true}, "then": ["#,
        "]}}",
        r#"{"wait": "1s"}"#,
    )
}

fn not_steps(n: usize) -> String {
    let when = nested(n, r#"{"not": "#, "}", r#"{"var": "x", "exists": true}"#);
    format!(r#"{{"if": {{"when": {when}, "then": [{{"wait": "1s"}}]}}}}"#)
}

fn json_steps(n: usize) -> String {
    let body = nested(n, "[", "]", "1");
    format!(r#"{{"request": {{"method": "GET", "path": "/", "json": {body}}}}}"#)
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
