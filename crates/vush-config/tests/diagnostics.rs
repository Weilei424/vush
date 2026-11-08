//! Error paths, locations and ordering through the public API.

use vush_config::{ConfigError, Format, parse};
use vush_core::plan::{Step, TemplatePart, VarRef};

fn errors(text: &str, format: Format) -> Vec<ConfigError> {
    parse(text, format).expect_err(text)
}

fn lines(errors: &[ConfigError]) -> Vec<u32> {
    errors
        .iter()
        .map(|e| e.location.expect("located").line)
        .collect()
}

#[test]
fn errors_in_aliased_content_point_to_the_definition() {
    let text = "\
schema_version: 1
name: shared
scenarios:
  - name: a
    load: { model: closed_vus, vus: 1, duration: 1s }
    steps:
      - request:
          method: GET
          path: /a
          timeout: &slow banana
      - request:
          method: GET
          path: /b
          timeout: *slow
";
    let errors = errors(text, Format::Yaml);
    let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "scenarios[0].steps[0].request.timeout",
            "scenarios[0].steps[1].request.timeout"
        ]
    );
    assert_eq!(lines(&errors), [10, 10]);
}

fn request_with_headers(headers: &str) -> String {
    format!(
        "\
schema_version: 1
name: n
scenarios:
  - name: a
    load: {{ model: closed_vus, vus: 1, duration: 1s }}
    steps:
      - request:
          method: GET
          url: http://localhost/
{headers}"
    )
}

#[test]
fn unquoted_reference_works_in_block_style_and_fails_cleanly_in_flow_style() {
    let block = request_with_headers(
        "          headers:\n            A: ${token}\n          extract: {token: {header: X}}\n",
    );
    let plan = parse(&block, Format::Yaml).expect("block style");
    let Step::Request(request) = &plan.scenarios()[0].steps[0] else {
        panic!("a request");
    };
    let (name, template) = &request.headers[0];
    assert_eq!(name, "A");
    assert_eq!(
        template.parts(),
        [TemplatePart::Var(VarRef::Vu("token".to_owned()))]
    );

    let flow = request_with_headers("          headers: { A: ${token} }\n");
    let errors = errors(&flow, Format::Yaml);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].location.expect("located").line, 10);
}

#[test]
fn both_formats_report_the_same_paths_and_messages() {
    // Each case is (top-level extras, request map); both are written in the
    // flow style that YAML and JSON share, inside each format's own file.
    let cases = [
        ("", r#"{"method": "GET", "path": "/", "nmae": 1}"#),
        ("", r#"{"method": "GET", "path": 5}"#),
        ("", r#"{"method": "GET", "path": "/", "timeout": "banana"}"#),
        ("", r#"{"path": "/"}"#),
        ("", r#"{"method": "GET", "path": "/", "url": "http://x"}"#),
        (
            r#""thresholds": [{"metric": "bogus", "lt": "1%"}]"#,
            r#"{"method": "GET", "path": "/"}"#,
        ),
    ];
    for (top, request) in cases {
        let json = format!(
            r#"{{"schema_version": 1, "name": "n", {top}{comma}"scenarios": [{{"name": "a", "load": {{"model": "closed_vus", "vus": 1, "duration": "1s"}}, "steps": [{{"request": {request}}}]}}]}}"#,
            comma = if top.is_empty() { "" } else { ", " },
        );
        let yaml = format!(
            "schema_version: 1\nname: n\n{top}\nscenarios:\n  - name: a\n    load: {{model: closed_vus, vus: 1, duration: 1s}}\n    steps:\n      - request: {request}\n"
        );
        let key = |errors: Vec<ConfigError>| -> Vec<(String, String)> {
            errors.into_iter().map(|e| (e.path, e.message)).collect()
        };
        let from_yaml = key(errors(&yaml, Format::Yaml));
        let from_json = key(errors(&json, Format::Json));
        assert!(!from_yaml.is_empty(), "{request}");
        assert_eq!(from_yaml, from_json, "{top} {request}");
    }
}

#[test]
fn all_problems_arrive_together_in_line_order() {
    let text = "\
schema_version: 1
name: n
bogus: 1
scenarios:
  - name: a
    load: { model: closed_vus, vus: 1, duration: banana }
    steps:
      - request:
          method: GET
          path: 5
      - wait: nope
      - request: { method: GET }
";
    let errors = errors(text, Format::Yaml);
    assert_eq!(errors.len(), 5, "{errors:?}");
    assert_eq!(lines(&errors), [3, 6, 10, 11, 12]);
}

#[test]
fn structural_problems_wait_for_shape_errors() {
    let with_key = "\
schema_version: 1
name: n
bogus: 1
scenarios:
  - name: a
    load: { model: closed_vus, vus: 1, duration: 1s }
    steps: []
";
    let errors_with = errors(with_key, Format::Yaml);
    assert_eq!(errors_with.len(), 1, "{errors_with:?}");
    assert!(errors_with[0].message.contains("unknown key"));

    let without = with_key.replace("bogus: 1\n", "");
    let errors_without = errors(&without, Format::Yaml);
    assert_eq!(errors_without.len(), 1, "{errors_without:?}");
    assert_eq!(errors_without[0].path, "scenarios[0].steps");
}

#[test]
fn merged_entries_report_at_their_source() {
    let text = "\
schema_version: 1
name: n
scenarios:
  - name: a
    load: { model: closed_vus, vus: 1, duration: 1s }
    steps:
      - request: &r
          method: GET
          path: /a
          bogus: 1
      - request:
          <<: *r
          path: /b
";
    let errors = errors(text, Format::Yaml);
    let found: Vec<(&str, u32)> = errors
        .iter()
        .map(|e| (e.path.as_str(), e.location.expect("located").line))
        .collect();
    assert_eq!(
        found,
        [
            ("scenarios[0].steps[0].request.bogus", 10),
            ("scenarios[0].steps[1].request.bogus", 10)
        ]
    );
}

#[test]
fn plan_errors_under_unusual_keys_are_located_exactly() {
    let request = |extra: &str| {
        format!(
            "\
schema_version: 1
name: n
base_url: http://localhost
scenarios:
  - name: a
    load: {{ model: closed_vus, vus: 1, duration: 1s }}
    steps:
      - request:
          method: GET
          path: /
{extra}"
        )
    };

    // A JSON key with a dot stays one segment, so the error lands on the
    // inner string and not on its parent.
    let text = request("          json:\n            \"a.b\":\n              c: \"${nope}\"\n");
    let found = errors(&text, Format::Yaml);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].path.ends_with("json.a.b.c"), "{}", found[0].path);
    assert_eq!(lines(&found), [13]);

    // A newline in a header name is written escaped in the path.
    let text = request("          headers:\n            \"X\\nY\": \"${nope}\"\n");
    let found = errors(&text, Format::Yaml);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].path.ends_with("headers.X\\nY"),
        "{}",
        found[0].path
    );
    assert!(!found[0].path.contains('\n'));
    assert_eq!(lines(&found), [12]);
}

#[test]
fn plan_errors_under_long_keys_that_share_a_prefix_find_their_own_key() {
    // Two 257-byte header names share their first 256 bytes. Paths keep only
    // a 256-byte prefix of each, but the error under the second one must
    // still point at the second one's line.
    let shared = "X".repeat(256);
    let text = format!(
        "\
schema_version: 1
name: n
scenarios:
  - name: a
    load: {{ model: closed_vus, vus: 1, duration: 1s }}
    steps:
      - request:
          method: GET
          url: http://localhost/
          headers:
            {shared}A: fine
            {shared}B: \"${{nope}}\"
"
    );
    let found = errors(&text, Format::Yaml);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].message.contains("sets this variable"), "{found:?}");
    assert_eq!(lines(&found), [12]);
}

#[test]
fn hints_only_offer_keys_valid_at_that_spot() {
    let file = |load: &str| {
        format!(
            "schema_version: 1\nname: n\nscenarios:\n  - name: a\n    load: {load}\n    steps:\n      - wait: 1s\n"
        )
    };
    for (load, want) in [
        (
            "{model: closed_vus, vus: 1, duration: 1s, strat: 0}",
            "unknown key `strat`",
        ),
        (
            "{model: closed_vus, stages: [{to: 1, over: 1s}], vu: 1}",
            "unknown key `vu`",
        ),
        (
            "{model: closed_vus, stages: [{to: 1, over: 1s}], strat: 0}",
            "unknown key `strat`, did you mean `start`?",
        ),
    ] {
        let found = errors(&file(load), Format::Yaml);
        assert_eq!(found.len(), 1, "{load}: {found:?}");
        assert_eq!(found[0].message, want, "{load}");
    }
}
