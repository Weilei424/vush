//! Plan rules reported through the public API, with paths and lines.

use vush_config::{ConfigError, Format, parse};

fn errors(text: &str, format: Format) -> Vec<ConfigError> {
    parse(text, format).expect_err(text)
}

const BASE: &str = "base_url: http://localhost\n";
const LOAD: &str = "    load: {model: closed_vus, vus: 1, duration: 1s}\n";

/// A file whose one mistake is in `top`, `scenario` (keys at indent 4,
/// including `load`) or `steps` (lines at indent 6).
fn file(top: &str, scenario: &str, steps: &str) -> String {
    format!(
        "schema_version: 1\nname: n\n{top}scenarios:\n  - name: a\n{scenario}    steps:\n{steps}"
    )
}

/// A request step; `extra` holds more request keys, at indent 10.
fn request(method: &str, extra: &str) -> String {
    format!("      - request:\n          method: {method}\n          path: /\n{extra}")
}

struct Row {
    kind: &'static str,
    text: String,
    path: &'static str,
    /// Text that appears on the line the error points at.
    needle: &'static str,
    fragment: &'static str,
}

fn row(
    kind: &'static str,
    text: String,
    path: &'static str,
    needle: &'static str,
    fragment: &'static str,
) -> Row {
    Row {
        kind,
        text,
        path,
        needle,
        fragment,
    }
}

fn rows() -> Vec<Row> {
    let get = |extra: &str| request("GET", extra);
    let top = |extra: &str| format!("{BASE}{extra}");
    let step_hook = |hook: &str, place: &str| {
        format!("{BASE}{hook}:\n  - request:\n      method: GET\n      path: /\n      {place}\n")
    };
    vec![
        row(
            "DurationTooLong",
            file(&top("http:\n  timeout: 721h\n"), LOAD, &get("")),
            "http.timeout",
            "721h",
            "at most 720h",
        ),
        row(
            "ZeroDuration",
            file(BASE, LOAD, &get("          timeout: 0s\n")),
            "scenarios[0].steps[0].request.timeout",
            "timeout: 0s",
            "longer than zero",
        ),
        row(
            "ZeroRate",
            file(
                BASE,
                "    load: {model: iteration_rate, rate: 0/s, duration: 1s, max_vus: 1}\n",
                &get(""),
            ),
            "scenarios[0].load.rate",
            "rate: 0/s",
            "above zero",
        ),
        row(
            "IdleRamp",
            file(
                BASE,
                "    load: {model: closed_vus, start: 0, stages: [{to: 0, over: 1s}]}\n",
                &get(""),
            ),
            "scenarios[0].load.stages",
            "stages:",
            "at its start",
        ),
        row(
            "ZeroLengthRamp",
            file(
                BASE,
                "    load: {model: closed_vus, start: 0, stages: [{to: 5, over: 0s}]}\n",
                &get(""),
            ),
            "scenarios[0].load.stages",
            "stages:",
            "in total",
        ),
        row(
            "TooFewAttempts",
            file(
                BASE,
                LOAD,
                &get("          retry:\n            attempts: 1\n"),
            ),
            "scenarios[0].steps[0].request.retry.attempts",
            "attempts: 1",
            "2 or more",
        ),
        row(
            "RetryNeedsOptIn",
            file(
                BASE,
                LOAD,
                &request("POST", "          retry: {attempts: 2}\n"),
            ),
            "scenarios[0].steps[0].request.retry",
            "retry:",
            "allow_non_idempotent",
        ),
        row(
            "StatusOutOfRange",
            file(BASE, LOAD, &get("          expect: {status: 99}\n")),
            "scenarios[0].steps[0].request.expect.status",
            "status: 99",
            "100 to 599",
        ),
        row(
            "EmptyStatus",
            file(BASE, LOAD, &get("          expect: {status: []}\n")),
            "scenarios[0].steps[0].request.expect.status",
            "status: []",
            "at least one code",
        ),
        row(
            "NoConditions",
            file(
                BASE,
                LOAD,
                "      - if:\n          when: {all: []}\n          then:\n            - wait: 1s\n",
            ),
            "scenarios[0].steps[0].if.when.all",
            "all: []",
            "at least one condition",
        ),
        row(
            "TemplateInName",
            file(BASE, LOAD, &get("          name: \"a${\"\n")),
            "scenarios[0].steps[0].request.name",
            "name: \"a${\"",
            "fixed metrics label",
        ),
        row(
            "PathNeedsBaseUrl",
            file("", LOAD, &get("")),
            "scenarios[0].steps[0].request.path",
            "path: /",
            "at the top of the file",
        ),
        row(
            "InvalidHeaderName",
            file(BASE, LOAD, &get("          headers: {\"Bad Name\": x}\n")),
            "scenarios[0].steps[0].request.headers.Bad Name",
            "Bad Name",
            "a header name can only use",
        ),
        row(
            "ForbiddenHeader",
            file(BASE, LOAD, &get("          headers: {Connection: close}\n")),
            "scenarios[0].steps[0].request.headers.Connection",
            "Connection:",
            "sets this header itself",
        ),
        row(
            "ControlCharacterInHeader",
            file(BASE, LOAD, &get("          headers: {X-A: \"a\\r\\nb\"}\n")),
            "scenarios[0].steps[0].request.headers.X-A",
            "X-A:",
            "control characters",
        ),
        row(
            "UnsupportedScheme",
            file(
                BASE,
                LOAD,
                "      - request:\n          method: GET\n          url: ftp://h/x\n",
            ),
            "scenarios[0].steps[0].request.url",
            "url: ftp://h/x",
            "http:// or https://",
        ),
        row(
            "CredentialsInUrl",
            file(
                BASE,
                LOAD,
                "      - request:\n          method: GET\n          url: https://u:p@h/x\n",
            ),
            "scenarios[0].steps[0].request.url",
            "url: https://u:p@h/x",
            "user name or password",
        ),
        row(
            "QueryInBaseUrl",
            file("base_url: \"https://h/?x=1\"\n", LOAD, &get("")),
            "base_url",
            "base_url:",
            "query or a fragment",
        ),
        row(
            "UnsupportedMethod",
            file(BASE, LOAD, &request("CONNECT", "")),
            "scenarios[0].steps[0].request.method",
            "method: CONNECT",
            "CONNECT isn't supported",
        ),
        row(
            "UnknownScenario",
            file(
                &top("thresholds:\n  - metric: error_rate\n    scenario: nope\n    lt: 1%\n"),
                LOAD,
                &get(""),
            ),
            "thresholds[0].scenario",
            "scenario: nope",
            "no scenario has this name",
        ),
        row(
            "InvalidVariableName",
            file(
                BASE,
                LOAD,
                &get("          extract:\n            a-b: {header: X}\n"),
            ),
            "scenarios[0].steps[0].request.extract.a-b",
            "a-b:",
            "must match",
        ),
        row(
            "ReservedVariableName",
            file(
                BASE,
                LOAD,
                &get("          extract:\n            env: {header: X}\n"),
            ),
            "scenarios[0].steps[0].request.extract.env",
            "env:",
            "are reserved",
        ),
        row(
            "VariableNotReadable (base_url)",
            file("base_url: \"${token}\"\n", LOAD, &get("")),
            "base_url",
            "${token}",
            "can only read `env.`",
        ),
        row(
            "VariableNotReadable (file teardown)",
            file(
                &step_hook("teardown", "headers: {A: \"${token}\"}"),
                LOAD,
                &get(""),
            ),
            "teardown[0].request.headers.A",
            "${token}",
            "can't be read here",
        ),
        row(
            "VariableNotSet",
            file(BASE, LOAD, &get("          headers: {A: \"${nope}\"}\n")),
            "scenarios[0].steps[0].request.headers.A",
            "${nope}",
            "sets this variable",
        ),
    ]
}

#[test]
fn each_rule_is_reported_with_its_path_and_line() {
    for row in rows() {
        let errors = errors(&row.text, Format::Yaml);
        assert_eq!(errors.len(), 1, "{}: {errors:?}\n{}", row.kind, row.text);
        let error = &errors[0];
        assert_eq!(error.path, row.path, "{}", row.kind);
        assert!(
            error.message.contains(row.fragment),
            "{}: {}",
            row.kind,
            error.message
        );
        let line = row
            .text
            .lines()
            .position(|l| l.contains(row.needle))
            .expect("needle is in the file")
            + 1;
        assert_eq!(
            error.location.expect("located").line as usize,
            line,
            "{}: {}",
            row.kind,
            row.text
        );
    }
}

#[test]
fn json_files_report_the_same_rules() {
    const BASE_URL: &str = r#""base_url": "http://localhost""#;
    const LOAD: &str = r#"{"model": "closed_vus", "vus": 1, "duration": "1s"}"#;
    const PLAIN: &str = r#"{"method": "GET", "path": "/"}"#;
    // Each case is (top-level entries, load, request); the text is valid
    // flow syntax in both formats.
    let cases: Vec<(Vec<&str>, &str, String)> = vec![
        (
            vec![BASE_URL, r#""http": {"timeout": "721h"}"#],
            LOAD,
            PLAIN.to_owned(),
        ),
        (
            vec![BASE_URL],
            r#"{"model": "closed_vus", "start": 0, "stages": [{"to": 0, "over": "1s"}]}"#,
            PLAIN.to_owned(),
        ),
        (
            vec![BASE_URL],
            LOAD,
            r#"{"method": "GET", "path": "/", "retry": {"attempts": 1}}"#.to_owned(),
        ),
        (
            vec![BASE_URL],
            LOAD,
            r#"{"method": "GET", "path": "/", "expect": {"status": []}}"#.to_owned(),
        ),
        (vec![], LOAD, PLAIN.to_owned()),
        (
            vec![
                BASE_URL,
                r#""thresholds": [{"metric": "error_rate", "scenario": "nope", "lt": "1%"}]"#,
            ],
            LOAD,
            PLAIN.to_owned(),
        ),
        (
            vec![BASE_URL],
            LOAD,
            r#"{"method": "GET", "path": "/", "headers": {"A": "${nope}"}}"#.to_owned(),
        ),
        (
            vec![BASE_URL],
            LOAD,
            r#"{"method": "GET", "path": "/", "headers": {"Bad Name": "x"}}"#.to_owned(),
        ),
        (
            vec![BASE_URL],
            LOAD,
            r#"{"method": "GET", "path": "/", "headers": {"Connection": "close"}}"#.to_owned(),
        ),
        (
            vec![BASE_URL],
            LOAD,
            r#"{"method": "GET", "path": "/", "headers": {"X-A": "a\r\nb"}}"#.to_owned(),
        ),
        (
            vec![BASE_URL],
            LOAD,
            r#"{"method": "GET", "url": "ftp://h/x"}"#.to_owned(),
        ),
        (
            vec![BASE_URL],
            LOAD,
            r#"{"method": "GET", "url": "https://u:p@h/x"}"#.to_owned(),
        ),
        (
            vec![r#""base_url": "https://h/?x=1""#],
            LOAD,
            PLAIN.to_owned(),
        ),
        (
            vec![BASE_URL],
            LOAD,
            r#"{"method": "CONNECT", "path": "/"}"#.to_owned(),
        ),
        (
            vec![BASE_URL],
            LOAD,
            r#"{"method": "GET", "path": "/", "extract": {"a-b": {"header": "X"}}}"#.to_owned(),
        ),
        (
            vec![BASE_URL],
            LOAD,
            r#"{"method": "GET", "path": "/", "extract": {"vu": {"header": "X"}}}"#.to_owned(),
        ),
    ];
    for (top, load, request) in cases {
        let json = format!(
            r#"{{"schema_version": 1, "name": "n", {top}{comma}"scenarios": [{{"name": "a", "load": {load}, "steps": [{{"request": {request}}}]}}]}}"#,
            top = top.join(", "),
            comma = if top.is_empty() { "" } else { ", " },
        );
        let yaml = format!(
            "schema_version: 1\nname: n\n{top}{newline}scenarios:\n  - name: a\n    load: {load}\n    steps:\n      - request: {request}\n",
            top = top.join("\n"),
            newline = if top.is_empty() { "" } else { "\n" },
        );
        let key = |errors: Vec<ConfigError>| -> Vec<(String, String)> {
            errors.into_iter().map(|e| (e.path, e.message)).collect()
        };
        let from_yaml = key(errors(&yaml, Format::Yaml));
        let from_json = key(errors(&json, Format::Json));
        assert_eq!(from_yaml.len(), 1, "{request}: {from_yaml:?}");
        assert_eq!(from_yaml, from_json, "{request}");
    }
}

#[test]
fn plan_rules_wait_for_a_clean_compile() {
    let with_typo = file(
        BASE,
        LOAD,
        &request("GET", "          nmae: x\n          retry: {attempts: 1}\n"),
    );
    let errors_with = errors(&with_typo, Format::Yaml);
    assert_eq!(errors_with.len(), 1, "{errors_with:?}");
    assert!(errors_with[0].message.contains("unknown key"));
    assert_eq!(errors_with[0].path, "scenarios[0].steps[0].request.nmae");

    let without = with_typo.replace("          nmae: x\n", "");
    let errors_without = errors(&without, Format::Yaml);
    assert_eq!(errors_without.len(), 1, "{errors_without:?}");
    assert_eq!(
        errors_without[0].path,
        "scenarios[0].steps[0].request.retry.attempts"
    );
    assert!(errors_without[0].message.contains("2 or more"));
}

#[test]
fn aliased_references_report_each_use_at_the_definition() {
    let text = "\
schema_version: 1
name: n
base_url: http://localhost
scenarios:
  - name: a
    load: {model: closed_vus, vus: 1, duration: 1s}
    steps:
      - request:
          method: GET
          path: /a
          headers: &h {A: \"${nope}\"}
      - request:
          method: GET
          path: /b
          headers: *h
      - request:
          method: GET
          path: /c
          headers: *h
";
    let errors = errors(text, Format::Yaml);
    let found: Vec<(&str, u32)> = errors
        .iter()
        .map(|e| (e.path.as_str(), e.location.expect("located").line))
        .collect();
    assert_eq!(
        found,
        [
            ("scenarios[0].steps[0].request.headers.A", 11),
            ("scenarios[0].steps[1].request.headers.A", 11),
            ("scenarios[0].steps[2].request.headers.A", 11),
        ]
    );
    assert!(
        errors
            .iter()
            .all(|e| e.message.contains("sets this variable"))
    );
}
