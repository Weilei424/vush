//! The two formats, and the YAML conveniences, give the same plans.

mod common;

use common::example_block;
use vush_config::{Format, parse};
use vush_core::plan::{Body, JsonNumber, JsonTemplate, Step, WorkloadPlan};

#[test]
fn aliases_and_merge_keys_equal_the_written_out_json() {
    let yaml = r#"
schema_version: 1
name: shared
scenarios:
  - name: a
    load: { model: closed_vus, vus: 1, duration: 1s }
    steps:
      - request: &login
          method: POST
          path: /login
          headers: &auth
            Authorization: "Bearer ${token}"
            Accept: application/json
      - request:
          method: GET
          path: /items
          headers:
            <<: *auth
            Accept: text/plain
            X-Trace: "1"
      - request:
          <<: *login
          path: /again
  - name: b
    load: { model: closed_vus, vus: 1, duration: 1s }
    steps:
      - request: *login
"#;
    let json = r#"{
  "schema_version": 1,
  "name": "shared",
  "scenarios": [
    {"name": "a",
     "load": {"model": "closed_vus", "vus": 1, "duration": "1s"},
     "steps": [
       {"request": {"method": "POST", "path": "/login",
          "headers": {"Authorization": "Bearer ${token}", "Accept": "application/json"}}},
       {"request": {"method": "GET", "path": "/items",
          "headers": {"Authorization": "Bearer ${token}", "Accept": "text/plain", "X-Trace": "1"}}},
       {"request": {"method": "POST", "path": "/again",
          "headers": {"Authorization": "Bearer ${token}", "Accept": "application/json"}}}
     ]},
    {"name": "b",
     "load": {"model": "closed_vus", "vus": 1, "duration": "1s"},
     "steps": [
       {"request": {"method": "POST", "path": "/login",
          "headers": {"Authorization": "Bearer ${token}", "Accept": "application/json"}}}
     ]}
  ]
}"#;
    let from_yaml = parse(yaml, Format::Yaml).expect("yaml");
    let from_json = parse(json, Format::Json).expect("json");
    assert_eq!(from_yaml, from_json);

    let Step::Request(second) = &from_yaml.scenarios()[0].steps[1] else {
        panic!("a request");
    };
    let names: Vec<&str> = second.headers.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["Authorization", "Accept", "X-Trace"]);
}

#[test]
fn crlf_and_bom_files_parse_to_the_same_plan() {
    for (lang, format) in [("yaml", Format::Yaml), ("json", Format::Json)] {
        let text = example_block(lang);
        let original = parse(text, format).expect("valid");
        let altered = format!("\u{feff}{}", text.replace('\n', "\r\n"));
        assert_eq!(parse(&altered, format).expect("valid"), original, "{lang}");
    }
}

fn with_body(body: &str, format: Format) -> WorkloadPlan {
    let text = match format {
        Format::Yaml => format!(
            "schema_version: 1\nname: n\nscenarios:\n  - name: s\n    load: {{model: closed_vus, vus: 1, duration: 1s}}\n    steps:\n      - request:\n          method: POST\n          path: /\n          json: {body}\n"
        ),
        Format::Json => format!(
            r#"{{"schema_version": 1, "name": "n", "scenarios": [{{"name": "s", "load": {{"model": "closed_vus", "vus": 1, "duration": "1s"}}, "steps": [{{"request": {{"method": "POST", "path": "/", "json": {body}}}}}]}}]}}"#
        ),
    };
    parse(&text, format).unwrap_or_else(|e| panic!("{text}\n{e:?}"))
}

fn json_body(plan: &WorkloadPlan) -> JsonTemplate {
    let Step::Request(request) = &plan.scenarios()[0].steps[0] else {
        panic!("a request");
    };
    let Some(Body::Json(body)) = &request.body else {
        panic!("a json body");
    };
    body.clone()
}

#[test]
fn both_formats_agree_on_numbers() {
    let body = r#"{"a": 9223372036854775807, "b": 18446744073709551615, "c": -9223372036854775808, "d": 0.5, "e": 1e3, "f": -0}"#;
    let from_yaml = with_body(body, Format::Yaml);
    let from_json = with_body(body, Format::Json);
    assert_eq!(from_yaml, from_json);
    let number = |n| JsonTemplate::Number(n);
    assert_eq!(
        json_body(&from_yaml),
        JsonTemplate::Object(vec![
            ("a".to_owned(), number(JsonNumber::Int(i64::MAX))),
            ("b".to_owned(), number(JsonNumber::UInt(u64::MAX))),
            ("c".to_owned(), number(JsonNumber::Int(i64::MIN))),
            ("d".to_owned(), number(JsonNumber::Float(0.5))),
            ("e".to_owned(), number(JsonNumber::Float(1000.0))),
            ("f".to_owned(), number(JsonNumber::Int(0))),
        ])
    );
}

#[test]
fn plain_yaml_words_stay_strings() {
    let yaml = with_body(
        "{a: yes, b: no, c: on, d: off, e: NO, f: 2001-01-01, g: 1.2.3}",
        Format::Yaml,
    );
    let json = with_body(
        r#"{"a": "yes", "b": "no", "c": "on", "d": "off", "e": "NO", "f": "2001-01-01", "g": "1.2.3"}"#,
        Format::Json,
    );
    assert_eq!(yaml, json);
}

#[test]
fn block_scalars_equal_json_strings() {
    let yaml = "schema_version: 1\nname: n\nscenarios:\n  - name: s\n    load: {model: closed_vus, vus: 1, duration: 1s}\n    steps:\n      - request:\n          method: POST\n          path: /\n          body: |\n            line one\n            line two\n";
    let json = r#"{"schema_version": 1, "name": "n", "scenarios": [{"name": "s", "load": {"model": "closed_vus", "vus": 1, "duration": "1s"}, "steps": [{"request": {"method": "POST", "path": "/", "body": "line one\nline two\n"}}]}]}"#;
    assert_eq!(
        parse(yaml, Format::Yaml).expect("yaml"),
        parse(json, Format::Json).expect("json")
    );
}
