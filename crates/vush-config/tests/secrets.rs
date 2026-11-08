//! No value from a file reaches an error message, a path or `Debug` output.

use vush_config::{Format, parse};

const SECRET: &str = "hunter2";

fn assert_never_quoted(text: &str, format: Format) {
    let errors = parse(text, format).expect_err(text);
    assert!(!errors.is_empty());
    for error in errors {
        assert!(!error.to_string().contains(SECRET), "{text}");
        assert!(!format!("{error:?}").contains(SECRET), "{text}");
    }
}

const REQUEST_PATH: &str = "scenarios[0].steps[0].request";

/// A valid file with one place to vary. `@` stands for the secret.
fn document(top: &str, scenario: &str, load: &str, step: &str) -> String {
    let load = if load.is_empty() {
        "{model: closed_vus, vus: 1, duration: 1s}"
    } else {
        load
    };
    let step = if step.is_empty() {
        "request: {method: GET, path: /}"
    } else {
        step
    };
    format!(
        "schema_version: 1\nname: n\n{top}\nscenarios:\n  - name: a\n    {scenario}\n    load: {load}\n    steps:\n      - {step}\n"
    )
    .replace('@', SECRET)
}

fn request(fields: &str) -> String {
    format!("request: {{{fields}}}")
}

#[test]
fn a_secret_in_any_value_never_reaches_an_error() {
    // Rows that name method, path or url replace that key in the base
    // request, so a duplicate-key error can't fire first. A row with a path
    // is checked to fail at exactly that key.
    let requests: &[(&str, Option<&str>)] = &[
        ("method: GET, path: /, timeout: @", Some("timeout")),
        ("method: GET, path: /, timout: @", Some("timout")),
        ("method: @, path: /", Some("method")),
        ("method: GET, path: /, required: @", Some("required")),
        ("method: GET, path: /, name: {a: @}", Some("name")),
        (r#"method: GET, path: "@${""#, Some("path")),
        (r#"method: GET, url: "@${""#, Some("url")),
        (r#"method: GET, path: /, headers: {A: "@${"}"#, None),
        (r#"method: GET, path: /, query: {a: "@${oops"}"#, None),
        (r#"method: GET, path: /, form: {a: "@${"}"#, None),
        (r#"method: GET, path: /, body: "@${""#, Some("body")),
        (r#"method: GET, path: /, json: {a: "@${"}"#, None),
        (r#"method: GET, path: /, json: ["@${"]"#, None),
        ("method: GET, path: /, headers: {A: [@]}", None),
        ("method: GET, path: /, query: {a: {b: @}}", None),
        (
            r#"method: GET, path: /, expect: {body_contains: "@${"}"#,
            None,
        ),
        ("method: GET, path: /, expect: {status: @}", None),
        (
            "method: GET, path: /, expect: {json: [{path: @, exists: true}]}",
            None,
        ),
        (
            r#"method: GET, path: /, expect: {json: [{path: "$.a", equals: "@${"}]}"#,
            None,
        ),
        (
            r#"method: GET, path: /, expect: {json: [{path: "$.a", contains: "@${"}]}"#,
            None,
        ),
        (
            r#"method: GET, path: /, expect: {json: [{path: "$.a", gt: @}]}"#,
            None,
        ),
        ("method: GET, path: /, extract: {x: {json_path: @}}", None),
        ("method: GET, path: /, retry: {attempts: @}", None),
        (
            "method: GET, path: /, retry: {attempts: 2, backoff: @}",
            None,
        ),
    ];
    for (fields, key) in requests {
        let text = document("", "", "", &request(fields));
        assert_never_quoted(&text, Format::Yaml);
        if let Some(key) = key {
            let errors = parse(&text, Format::Yaml).expect_err(&text);
            assert!(
                errors
                    .iter()
                    .any(|e| e.path == format!("{REQUEST_PATH}.{key}")),
                "{fields}: {errors:?}"
            );
        }
    }

    let others: &[(&str, &str, &str, &str)] = &[
        // top, scenario, load, step
        (r#"base_url: "@${""#, "", "", ""),
        ("http: {version: @}", "", "", ""),
        ("http: {pool: @}", "", "", ""),
        ("http: {timeout: @}", "", "", ""),
        ("http: {max_response_size: @}", "", "", ""),
        ("limits: {max_request_rate: @/s}", "", "", ""),
        ("limits: {hook_timeout: @}", "", "", ""),
        ("thresholds: [{metric: @, lt: 1%}]", "", "", ""),
        ("thresholds: [{metric: error_rate, lt: @}]", "", "", ""),
        ("thresholds: [{metric: request_rate, lt: @}]", "", "", ""),
        (
            "thresholds: [{metric: http_request_duration, stat: @, lt: 1s}]",
            "",
            "",
            "",
        ),
        (
            "thresholds: [{metric: http_request_duration, stat: p95, lt: @}]",
            "",
            "",
            "",
        ),
        ("", "state: @", "", ""),
        ("", "", "{model: @, vus: 1, duration: 1s}", ""),
        (
            "",
            "",
            "{model: request_rate, vus: 1, rate: @/s, duration: 1s}",
            "",
        ),
        ("", "", "{model: closed_vus, vus: 1, duration: @}", ""),
        (
            "",
            "",
            "",
            r#"if: {when: {var: "@-x", exists: true}, then: [{wait: 1s}]}"#,
        ),
    ];
    for (top, scenario, load, step) in others {
        assert_never_quoted(&document(top, scenario, load, step), Format::Yaml);
    }
}

#[test]
fn ambiguous_numbers_are_never_quoted() {
    let sentinel = "2badc0de";
    let text = document(
        "",
        "",
        "",
        "request: {method: GET, path: /, json: {a: 0x2badc0de}}",
    );
    let errors = parse(&text, Format::Yaml).expect_err(&text);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("looks like a number")),
        "{errors:?}"
    );
    for error in errors {
        assert!(!error.to_string().contains(sentinel));
        assert!(!format!("{error:?}").contains(sentinel));
    }
}

#[test]
fn syntax_errors_never_quote_the_file() {
    for text in [r#"a: "@\q""#, "a: [@", "a: @: b"] {
        assert_never_quoted(&text.replace('@', SECRET), Format::Yaml);
    }
    for text in [
        r#"{"a": "@\q"}"#,
        r#"{"a": @}"#,
        r#"{"a": "@"#,
        r#"{"a": 1, @}"#,
    ] {
        assert_never_quoted(&text.replace('@', SECRET), Format::Json);
    }
}

#[test]
fn duplicate_scenario_names_are_never_quoted() {
    let text = "\
schema_version: 1
name: n
scenarios:
  - name: hunter2
    load: {model: closed_vus, vus: 1, duration: 1s}
    steps: [{wait: 1s}]
  - name: hunter2
    load: {model: closed_vus, vus: 1, duration: 1s}
    steps: [{wait: 1s}]
";
    assert_never_quoted(text, Format::Yaml);
    let errors = parse(text, Format::Yaml).expect_err(text);
    assert!(
        errors.iter().any(|e| e.path == "scenarios[1].name"),
        "{errors:?}"
    );
}

#[test]
fn colon_typos_in_unquoted_keys_never_reach_an_error() {
    for fields in [
        "method: GET, path: /, headers: {Authorization:Bearer @}",
        "method: GET, path: /, query: {api_key:@}",
        "method: GET, path: /, json: {password:@}",
        "method: GET, path: /, form: {token:@}",
    ] {
        let text = document("", "", "", &request(fields));
        assert_never_quoted(&text, Format::Yaml);
    }
}

#[test]
fn validation_errors_never_quote_the_file() {
    const BASE: &str = "base_url: http://localhost\n";
    let header = r#"headers: {A: "Bearer @"}"#;
    let plain = |extra: &str| request(&format!("method: GET, path: /, {header}{extra}"));
    // Each row is (top, scenario, load, step, fragment). `@` is the secret.
    let owned: Vec<(String, &str, &str, String, &str)> = vec![
        (
            BASE.into(),
            "",
            "",
            plain(", timeout: 721h"),
            "at most 720h",
        ),
        (
            BASE.into(),
            "",
            "",
            plain(", timeout: 0s"),
            "longer than zero",
        ),
        (
            BASE.into(),
            "",
            "{model: iteration_rate, rate: 0/s, duration: 1s, max_vus: 1}",
            plain(""),
            "above zero",
        ),
        (
            BASE.into(),
            "",
            "{model: closed_vus, start: 0, stages: [{to: 0, over: 1s}]}",
            plain(""),
            "at its start",
        ),
        (
            BASE.into(),
            "",
            "{model: closed_vus, start: 0, stages: [{to: 5, over: 0s}]}",
            plain(""),
            "in total",
        ),
        (
            BASE.into(),
            "",
            "",
            plain(", retry: {attempts: 1}"),
            "2 or more",
        ),
        (
            BASE.into(),
            "",
            "",
            request(&format!(
                "method: POST, path: /, {header}, retry: {{attempts: 2}}"
            )),
            "allow_non_idempotent",
        ),
        (
            BASE.into(),
            "",
            "",
            plain(", expect: {status: 99}"),
            "100 to 599",
        ),
        (
            BASE.into(),
            "",
            "",
            plain(", expect: {status: []}"),
            "at least one code",
        ),
        (
            BASE.into(),
            "",
            "",
            format!(
                "if: {{when: {{all: []}}, then: [{{request: {{method: GET, path: /, {header}}}}}]}}"
            ),
            "at least one condition",
        ),
        (
            BASE.into(),
            "",
            "",
            request(r#"method: GET, path: /, name: "@${""#),
            "fixed metrics label",
        ),
        (
            String::new(),
            "",
            "",
            request("method: GET, path: /@"),
            "at the top of the file",
        ),
        (
            format!("{BASE}thresholds: [{{metric: error_rate, scenario: @, lt: 1%}}]"),
            "",
            "",
            String::new(),
            "no scenario has this name",
        ),
        (
            BASE.into(),
            "",
            "",
            plain(r#", extract: {"a-b": {header: "@"}}"#),
            "must match",
        ),
        (
            BASE.into(),
            "",
            "",
            plain(r#", extract: {env: {header: "@"}}"#),
            "are reserved",
        ),
        (
            r#"base_url: "${@}""#.into(),
            "",
            "",
            String::new(),
            "only read `env.`",
        ),
        (
            format!(
                "{BASE}teardown:\n  - request: {{method: GET, path: /, headers: {{A: \"${{@}}\"}}}}"
            ),
            "",
            "",
            String::new(),
            "can't be read here",
        ),
        (
            BASE.into(),
            "",
            "",
            request(r#"method: GET, path: /, headers: {A: "${@}"}"#),
            "sets this variable",
        ),
        (
            BASE.into(),
            "",
            "",
            request(&format!(
                "method: POST, path: /, {header}, json: [{}]",
                vec![r#""@ ${nope}""#; 150].join(", ")
            )),
            "too many errors",
        ),
    ];
    for (top, scenario, load, step, fragment) in &owned {
        let text = document(top, scenario, load, step);
        let errors = parse(&text, Format::Yaml).expect_err(&text);
        assert!(
            errors.iter().any(|e| e.message.contains(fragment)),
            "{fragment}: {errors:?}\n{text}"
        );
        assert_never_quoted(&text, Format::Yaml);
    }
}
