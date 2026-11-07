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
    let block = request_with_headers("          headers:\n            A: ${token}\n");
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
