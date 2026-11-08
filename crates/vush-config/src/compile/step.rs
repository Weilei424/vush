//! Step lists: requests, waits, `if` and `repeat`.

use std::num::NonZeroU32;
use std::time::Duration;

use vush_core::plan::{
    Body, Comparison, Condition, DEFAULT_REPEAT_DEADLINE, DEFAULT_RETRY_BACKOFF, Expect,
    ExtractSource, Extraction, HttpMethod, IfStep, JsonAssertion, JsonPath, JsonTemplate,
    RepeatStep, RequestStep, RequestTarget, RetryPolicy, Scalar, StatusMatch, Step, Template,
    VarRef, parse_duration,
};

use super::reader::{MapReader, boolean, child, integer, item, list, number, parsed, string};
use crate::error::{Errors, display_key};
use crate::node::{Node, NodeKind};

const U32_MAX: u64 = u32::MAX as u64;

/// Reads a list of steps. Every step is read, so one bad step doesn't hide
/// another. Requests without a `timeout` get `default_timeout`.
pub(super) fn steps(
    node: &Node,
    path: &str,
    default_timeout: Duration,
    errors: &mut Errors,
) -> Option<Vec<Step>> {
    let items = list(node, path, errors)?;
    let mut out = Vec::with_capacity(items.len());
    let mut ok = true;
    for (index, step_node) in items.iter().enumerate() {
        match step(step_node, &item(path, index), default_timeout, errors) {
            Some(built) => out.push(built),
            None => ok = false,
        }
    }
    ok.then_some(out)
}

/// A step is a map with exactly one key, which names its kind.
fn step(node: &Node, path: &str, default_timeout: Duration, errors: &mut Errors) -> Option<Step> {
    let NodeKind::Map(entries) = &node.kind else {
        // Reports the wrong type, or says nothing for an invalid node.
        MapReader::new(node, path, errors);
        return None;
    };
    let [entry] = entries.as_slice() else {
        errors.push(
            path,
            Some(node.location),
            "a step needs exactly one key: request, wait, if or repeat",
        );
        return None;
    };
    let key_path = child(path, &entry.key);
    let value = &entry.value;
    match entry.key.as_str() {
        "request" => {
            request(value, &key_path, default_timeout, errors).map(|r| Step::Request(Box::new(r)))
        }
        "wait" => parsed(value, &key_path, parse_duration, errors).map(Step::Wait),
        "if" => if_step(value, &key_path, default_timeout, errors).map(Step::If),
        "repeat" => repeat(value, &key_path, default_timeout, errors).map(Step::Repeat),
        _ => {
            errors.push(
                &key_path,
                Some(entry.key_location),
                format!("unknown key `{}`", display_key(&entry.key)),
            );
            None
        }
    }
}

/// An optional key: `Some(None)` when absent, `None` when present and bad.
fn optional<'a, T>(
    map: &mut MapReader<'a>,
    key: &str,
    errors: &mut Errors,
    read: impl FnOnce(&'a Node, &mut Errors) -> Option<T>,
) -> Option<Option<T>> {
    match map.optional(key) {
        Some(node) => read(node, errors).map(Some),
        None => Some(None),
    }
}

fn request(
    node: &Node,
    path: &str,
    default_timeout: Duration,
    errors: &mut Errors,
) -> Option<RequestStep> {
    let mut map = MapReader::new(node, path, errors)?;
    let name = optional(&mut map, "name", errors, |n, e| {
        string(n, &child(path, "name"), e).map(str::to_owned)
    });
    let method = map
        .required("method", errors)
        .and_then(|n| parsed(n, &child(path, "method"), str::parse::<HttpMethod>, errors));
    let target = match map.exactly_one(&["path", "url"], errors) {
        Some((key, n)) => {
            parsed(n, &child(path, key), str::parse::<Template>, errors).map(|t| match key {
                "path" => RequestTarget::Path(t),
                _ => RequestTarget::Url(t),
            })
        }
        None => None,
    };
    let query = optional(&mut map, "query", errors, |n, e| {
        pairs(n, &child(path, "query"), e)
    });
    let headers = optional(&mut map, "headers", errors, |n, e| {
        pairs(n, &child(path, "headers"), e)
    });
    let body = body(&mut map, path, errors);
    let timeout = optional(&mut map, "timeout", errors, |n, e| {
        parsed(n, &child(path, "timeout"), parse_duration, e)
    });
    let required = optional(&mut map, "required", errors, |n, e| {
        boolean(n, &child(path, "required"), e)
    });
    let retry = optional(&mut map, "retry", errors, |n, e| {
        retry(n, &child(path, "retry"), e)
    });
    let expect_value = optional(&mut map, "expect", errors, |n, e| {
        expect(n, &child(path, "expect"), e)
    });
    let extract = optional(&mut map, "extract", errors, |n, e| {
        extract(n, &child(path, "extract"), e)
    });
    map.finish(errors);
    Some(RequestStep {
        name: name?,
        method: method?,
        target: target?,
        query: query?.unwrap_or_default(),
        headers: headers?.unwrap_or_default(),
        body: body?,
        timeout: timeout?.unwrap_or(default_timeout),
        required: required?.unwrap_or(true),
        retry: retry?,
        expect: expect_value?.unwrap_or_default(),
        extract: extract?.unwrap_or_default(),
    })
}

/// A map of free-form keys to template strings, in file order.
fn pairs(node: &Node, path: &str, errors: &mut Errors) -> Option<Vec<(String, Template)>> {
    let NodeKind::Map(entries) = &node.kind else {
        MapReader::new(node, path, errors);
        return None;
    };
    let mut out = Vec::with_capacity(entries.len());
    let mut ok = true;
    for entry in entries {
        let value_path = child(path, &entry.key);
        match parsed(&entry.value, &value_path, str::parse::<Template>, errors) {
            Some(t) => out.push((entry.key.clone(), t)),
            None => ok = false,
        }
    }
    ok.then_some(out)
}

/// `Some(None)` when there's no body, `None` when the body is bad.
fn body(map: &mut MapReader<'_>, path: &str, errors: &mut Errors) -> Option<Option<Body>> {
    match map.at_most_one(&["json", "body", "form"], errors) {
        None => Some(None),
        Some(("json", n)) => {
            json_template(n, &child(path, "json"), errors).map(|j| Some(Body::Json(j)))
        }
        Some(("body", n)) => parsed(n, &child(path, "body"), str::parse::<Template>, errors)
            .map(|t| Some(Body::Text(t))),
        Some((_, n)) => pairs(n, &child(path, "form"), errors).map(|f| Some(Body::Form(f))),
    }
}

fn json_template(node: &Node, path: &str, errors: &mut Errors) -> Option<JsonTemplate> {
    match &node.kind {
        NodeKind::Null => Some(JsonTemplate::Null),
        NodeKind::Bool(b) => Some(JsonTemplate::Bool(*b)),
        NodeKind::Number(n) => Some(JsonTemplate::Number(*n)),
        NodeKind::String(_) => {
            parsed(node, path, str::parse::<Template>, errors).map(JsonTemplate::String)
        }
        NodeKind::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            let mut ok = true;
            for (index, n) in items.iter().enumerate() {
                match json_template(n, &item(path, index), errors) {
                    Some(v) => out.push(v),
                    None => ok = false,
                }
            }
            ok.then_some(JsonTemplate::Array(out))
        }
        NodeKind::Map(entries) => {
            let mut out = Vec::with_capacity(entries.len());
            let mut ok = true;
            for entry in entries {
                match json_template(&entry.value, &child(path, &entry.key), errors) {
                    Some(v) => out.push((entry.key.clone(), v)),
                    None => ok = false,
                }
            }
            ok.then_some(JsonTemplate::Object(out))
        }
        NodeKind::Invalid => None,
    }
}

fn retry(node: &Node, path: &str, errors: &mut Errors) -> Option<RetryPolicy> {
    let mut map = MapReader::new(node, path, errors)?;
    let attempts = map
        .required("attempts", errors)
        .and_then(|n| integer(n, &child(path, "attempts"), 0, U32_MAX, errors))
        .and_then(|v| u32::try_from(v).ok());
    let backoff = optional(&mut map, "backoff", errors, |n, e| {
        parsed(n, &child(path, "backoff"), parse_duration, e)
    });
    let allow = optional(&mut map, "allow_non_idempotent", errors, |n, e| {
        boolean(n, &child(path, "allow_non_idempotent"), e)
    });
    map.finish(errors);
    Some(RetryPolicy {
        attempts: attempts?,
        backoff: backoff?.unwrap_or(DEFAULT_RETRY_BACKOFF),
        allow_non_idempotent: allow?.unwrap_or(false),
    })
}

fn expect(node: &Node, path: &str, errors: &mut Errors) -> Option<Expect> {
    let mut map = MapReader::new(node, path, errors)?;
    let status_value = optional(&mut map, "status", errors, |n, e| {
        status(n, &child(path, "status"), e)
    });
    let body_contains = optional(&mut map, "body_contains", errors, |n, e| {
        parsed(n, &child(path, "body_contains"), str::parse::<Template>, e)
    });
    let json = optional(&mut map, "json", errors, |n, e| {
        assertions(n, &child(path, "json"), e)
    });
    map.finish(errors);
    Some(Expect {
        status: status_value?.unwrap_or(StatusMatch::AnySuccess),
        body_contains: body_contains?,
        json: json?.unwrap_or_default(),
    })
}

fn status(node: &Node, path: &str, errors: &mut Errors) -> Option<StatusMatch> {
    let code = |n: &Node, p: &str, e: &mut Errors| {
        integer(n, p, 0, u64::from(u16::MAX), e).and_then(|v| u16::try_from(v).ok())
    };
    if let NodeKind::List(items) = &node.kind {
        let mut out = Vec::with_capacity(items.len());
        let mut ok = true;
        for (index, n) in items.iter().enumerate() {
            match code(n, &item(path, index), errors) {
                Some(v) => out.push(v),
                None => ok = false,
            }
        }
        return ok.then_some(StatusMatch::OneOf(out));
    }
    code(node, path, errors).map(|v| StatusMatch::OneOf(vec![v]))
}

fn assertions(node: &Node, path: &str, errors: &mut Errors) -> Option<Vec<JsonAssertion>> {
    let items = list(node, path, errors)?;
    let mut out = Vec::with_capacity(items.len());
    let mut ok = true;
    for (index, n) in items.iter().enumerate() {
        match assertion(n, &item(path, index), errors) {
            Some(a) => out.push(a),
            None => ok = false,
        }
    }
    ok.then_some(out)
}

fn assertion(node: &Node, path: &str, errors: &mut Errors) -> Option<JsonAssertion> {
    let mut map = MapReader::new(node, path, errors)?;
    let json_path = map
        .required("path", errors)
        .and_then(|n| parsed(n, &child(path, "path"), str::parse::<JsonPath>, errors));
    let op = comparison(&mut map, path, errors);
    map.finish(errors);
    Some(JsonAssertion {
        path: json_path?,
        op: op?,
    })
}

fn extract(node: &Node, path: &str, errors: &mut Errors) -> Option<Vec<Extraction>> {
    let NodeKind::Map(entries) = &node.kind else {
        MapReader::new(node, path, errors);
        return None;
    };
    let mut out = Vec::with_capacity(entries.len());
    let mut ok = true;
    for entry in entries {
        let var_path = child(path, &entry.key);
        let Some(mut map) = MapReader::new(&entry.value, &var_path, errors) else {
            ok = false;
            continue;
        };
        let source = match map.exactly_one(&["json_path", "header"], errors) {
            Some(("json_path", n)) => parsed(
                n,
                &child(&var_path, "json_path"),
                str::parse::<JsonPath>,
                errors,
            )
            .map(ExtractSource::JsonPath),
            Some((_, n)) => string(n, &child(&var_path, "header"), errors)
                .map(|h| ExtractSource::Header(h.to_owned())),
            None => None,
        };
        map.finish(errors);
        match source {
            Some(source) => out.push(Extraction {
                var: entry.key.clone(),
                source,
            }),
            None => ok = false,
        }
    }
    ok.then_some(out)
}

/// Reads the one comparison key. The caller has already taken `path` or `var`.
const COMPARISON_KEYS: [&str; 8] = [
    "exists",
    "equals",
    "not_equals",
    "gt",
    "gte",
    "lt",
    "lte",
    "contains",
];

fn comparison(map: &mut MapReader<'_>, path: &str, errors: &mut Errors) -> Option<Comparison> {
    const KEYS: [&str; 8] = COMPARISON_KEYS;
    let (key, n) = map.exactly_one(&KEYS, errors)?;
    let p = child(path, key);
    match key {
        "exists" => boolean(n, &p, errors).map(Comparison::Exists),
        "equals" => scalar(n, &p, errors).map(Comparison::Equals),
        "not_equals" => scalar(n, &p, errors).map(Comparison::NotEquals),
        "gt" => number(n, &p, errors).map(Comparison::Gt),
        "gte" => number(n, &p, errors).map(Comparison::Gte),
        "lt" => number(n, &p, errors).map(Comparison::Lt),
        "lte" => number(n, &p, errors).map(Comparison::Lte),
        _ => parsed(n, &p, str::parse::<Template>, errors).map(Comparison::Contains),
    }
}

fn scalar(node: &Node, path: &str, errors: &mut Errors) -> Option<Scalar> {
    let found = match &node.kind {
        NodeKind::Null => return Some(Scalar::Null),
        NodeKind::Bool(b) => return Some(Scalar::Bool(*b)),
        NodeKind::Number(n) => return Some(Scalar::Number(*n)),
        NodeKind::String(_) => {
            return parsed(node, path, str::parse::<Template>, errors).map(Scalar::String);
        }
        NodeKind::List(_) => "a list",
        NodeKind::Map(_) => "a map",
        NodeKind::Invalid => return None,
    };
    errors.push(
        path,
        Some(node.location),
        format!("expected a string, number, boolean or null, found {found}"),
    );
    None
}

fn condition(node: &Node, path: &str, errors: &mut Errors) -> Option<Condition> {
    let mut map = MapReader::new(node, path, errors)?;
    let built = match map.exactly_one(&["var", "all", "any", "not"], errors) {
        Some(("var", n)) => {
            let var = parsed(n, &child(path, "var"), str::parse::<VarRef>, errors);
            let op = comparison(&mut map, path, errors);
            match (var, op) {
                (Some(var), Some(op)) => Some(Condition::Compare { var, op }),
                _ => None,
            }
        }
        Some(("all", n)) => conditions(n, &child(path, "all"), errors).map(Condition::All),
        Some(("any", n)) => conditions(n, &child(path, "any"), errors).map(Condition::Any),
        Some((_, n)) => {
            condition(n, &child(path, "not"), errors).map(|c| Condition::Not(Box::new(c)))
        }
        None => None,
    };
    // A losing `var` was already reported as a conflict; its comparison keys
    // are not unknown.
    if map.has("var") {
        map.ignore(&COMPARISON_KEYS);
    }
    map.finish(errors);
    built
}

fn conditions(node: &Node, path: &str, errors: &mut Errors) -> Option<Vec<Condition>> {
    let items = list(node, path, errors)?;
    let mut out = Vec::with_capacity(items.len());
    let mut ok = true;
    for (index, n) in items.iter().enumerate() {
        match condition(n, &item(path, index), errors) {
            Some(c) => out.push(c),
            None => ok = false,
        }
    }
    ok.then_some(out)
}

fn if_step(
    node: &Node,
    path: &str,
    default_timeout: Duration,
    errors: &mut Errors,
) -> Option<IfStep> {
    let mut map = MapReader::new(node, path, errors)?;
    let when = map
        .required("when", errors)
        .and_then(|n| condition(n, &child(path, "when"), errors));
    let then = map
        .required("then", errors)
        .and_then(|n| steps(n, &child(path, "then"), default_timeout, errors));
    let otherwise = optional(&mut map, "else", errors, |n, e| {
        steps(n, &child(path, "else"), default_timeout, e)
    });
    map.finish(errors);
    Some(IfStep {
        when: when?,
        then: then?,
        otherwise: otherwise?.unwrap_or_default(),
    })
}

fn repeat(
    node: &Node,
    path: &str,
    default_timeout: Duration,
    errors: &mut Errors,
) -> Option<RepeatStep> {
    let mut map = MapReader::new(node, path, errors)?;
    let count = map
        .required("count", errors)
        .and_then(|n| integer(n, &child(path, "count"), 1, U32_MAX, errors))
        .and_then(|v| NonZeroU32::new(u32::try_from(v).ok()?));
    let deadline = optional(&mut map, "deadline", errors, |n, e| {
        parsed(n, &child(path, "deadline"), parse_duration, e)
    });
    let body = map
        .required("steps", errors)
        .and_then(|n| steps(n, &child(path, "steps"), default_timeout, errors));
    map.finish(errors);
    Some(RepeatStep {
        count: count?,
        deadline: deadline?.unwrap_or(DEFAULT_REPEAT_DEADLINE),
        steps: body?,
    })
}

#[cfg(test)]
mod tests {
    use vush_core::plan::{JsonNumber::*, Scalar as S};

    use super::*;
    use crate::compile::testing::{assert_fails, run};

    fn tpl(text: &str) -> Template {
        text.parse().unwrap()
    }

    fn ok(yaml: &str) -> Vec<Step> {
        let (value, errors) = run(yaml, |node, errors| {
            steps(node, "steps", Duration::from_secs(7), errors)
        });
        assert!(errors.is_empty(), "{yaml}: {errors:?}");
        value.expect("no steps")
    }

    fn fails(yaml: &str, path: &str, fragment: &str) {
        assert_fails(yaml, path, fragment, |node, errors| {
            steps(node, "steps", Duration::from_secs(7), errors)
        });
    }

    fn req(fields: &str) -> String {
        format!("[{{request: {{method: GET, path: /a, {fields}}}}}]")
    }

    /// The request of the only step.
    fn only_request(yaml: &str) -> RequestStep {
        match ok(yaml).remove(0) {
            Step::Request(r) => *r,
            other => panic!("not a request: {other:?}"),
        }
    }

    #[test]
    fn a_minimal_request_takes_every_default() {
        assert_eq!(
            ok("[{request: {method: GET, path: /a}}]"),
            [Step::Request(Box::new(RequestStep {
                name: None,
                method: "GET".parse().unwrap(),
                target: RequestTarget::Path("/a".parse().unwrap()),
                query: vec![],
                headers: vec![],
                body: None,
                timeout: Duration::from_secs(7),
                required: true,
                retry: None,
                expect: Expect::default(),
                extract: vec![],
            }))]
        );
    }

    #[test]
    fn a_full_request_maps_every_key() {
        let yaml = r#"
- request:
    name: full
    method: POST
    url: https://x/${id}
    query: {q: "${q}", r: x}
    headers: {B: "2", A: "1"}
    body: "hi ${name}"
    timeout: 5s
    required: false
    retry: {attempts: 3, backoff: 250ms, allow_non_idempotent: true}
    expect:
      status: [200, 201]
      body_contains: ok
      json:
        - {path: "$.a", exists: true}
    extract:
      tok: {json_path: "$.t"}
      tag: {header: ETag}
"#;
        assert_eq!(
            ok(yaml),
            [Step::Request(Box::new(RequestStep {
                name: Some("full".into()),
                method: "POST".parse().unwrap(),
                target: RequestTarget::Url(tpl("https://x/${id}")),
                query: vec![("q".into(), tpl("${q}")), ("r".into(), tpl("x"))],
                headers: vec![("B".into(), tpl("2")), ("A".into(), tpl("1"))],
                body: Some(Body::Text(tpl("hi ${name}"))),
                timeout: Duration::from_secs(5),
                required: false,
                retry: Some(RetryPolicy {
                    attempts: 3,
                    backoff: Duration::from_millis(250),
                    allow_non_idempotent: true,
                }),
                expect: Expect {
                    status: StatusMatch::OneOf(vec![200, 201]),
                    body_contains: Some(tpl("ok")),
                    json: vec![JsonAssertion {
                        path: "$.a".parse().unwrap(),
                        op: Comparison::Exists(true),
                    }],
                },
                extract: vec![
                    Extraction {
                        var: "tok".into(),
                        source: ExtractSource::JsonPath("$.t".parse().unwrap()),
                    },
                    Extraction {
                        var: "tag".into(),
                        source: ExtractSource::Header("ETag".into()),
                    },
                ],
            }))]
        );
    }

    #[test]
    fn bodies_take_each_form() {
        for (fields, expected) in [
            (
                "json: {a: 1}",
                JsonTemplate::Object(vec![("a".into(), JsonTemplate::Number(Int(1)))]),
            ),
            ("json: null", JsonTemplate::Null),
            ("json: 5", JsonTemplate::Number(Int(5))),
            ("json: \"${v}\"", JsonTemplate::String(tpl("${v}"))),
        ] {
            assert_eq!(
                only_request(&req(fields)).body,
                Some(Body::Json(expected)),
                "{fields}"
            );
        }
        assert_eq!(
            only_request(&req("body: text")).body,
            Some(Body::Text(tpl("text")))
        );
        assert_eq!(
            only_request(&req("form: {b: \"1\", a: \"${x}\"}")).body,
            Some(Body::Form(vec![
                ("b".into(), tpl("1")),
                ("a".into(), tpl("${x}"))
            ]))
        );
    }

    #[test]
    fn json_bodies_keep_order_exact_numbers_and_templates() {
        let body = only_request(&req(
            "json: {b: 1, a: \"x${y}\", c: [true, null, 18446744073709551615, 0.5], d: {}}",
        ))
        .body;
        assert_eq!(
            body,
            Some(Body::Json(JsonTemplate::Object(vec![
                ("b".into(), JsonTemplate::Number(Int(1))),
                ("a".into(), JsonTemplate::String(tpl("x${y}"))),
                (
                    "c".into(),
                    JsonTemplate::Array(vec![
                        JsonTemplate::Bool(true),
                        JsonTemplate::Null,
                        JsonTemplate::Number(UInt(u64::MAX)),
                        JsonTemplate::Number(Float(0.5)),
                    ])
                ),
                ("d".into(), JsonTemplate::Object(vec![])),
            ])))
        );
    }

    #[test]
    fn status_takes_each_form() {
        assert_eq!(
            only_request(&req("")).expect.status,
            StatusMatch::AnySuccess
        );
        for (fields, expected) in [
            ("expect: {status: 200}", vec![200]),
            ("expect: {status: [200, 201]}", vec![200, 201]),
            ("expect: {status: []}", vec![]),
        ] {
            assert_eq!(
                only_request(&req(fields)).expect.status,
                StatusMatch::OneOf(expected),
                "{fields}"
            );
        }
    }

    #[test]
    fn retry_takes_its_defaults() {
        assert_eq!(
            only_request(&req("retry: {attempts: 3}")).retry,
            Some(RetryPolicy {
                attempts: 3,
                backoff: DEFAULT_RETRY_BACKOFF,
                allow_non_idempotent: false,
            })
        );
        assert!(only_request(&req("retry: {attempts: 1}")).retry.is_some());
    }

    #[test]
    fn compile_keeps_a_request_name_as_written() {
        let r = only_request(&req("name: \"a${b}\""));
        assert_eq!(r.name.as_deref(), Some("a${b}"));
    }

    fn rows() -> Vec<(&'static str, Comparison)> {
        vec![
            ("exists: true", Comparison::Exists(true)),
            ("equals: abc", Comparison::Equals(S::String(tpl("abc")))),
            (
                "equals: \"${x}\"",
                Comparison::Equals(S::String(tpl("${x}"))),
            ),
            ("equals: 2", Comparison::Equals(S::Number(Int(2)))),
            ("equals: true", Comparison::Equals(S::Bool(true))),
            ("equals: null", Comparison::Equals(S::Null)),
            (
                "not_equals: 2.5",
                Comparison::NotEquals(S::Number(Float(2.5))),
            ),
            ("gt: 0", Comparison::Gt(Int(0))),
            ("gte: 1.5", Comparison::Gte(Float(1.5))),
            ("lt: -1", Comparison::Lt(Int(-1))),
            ("lte: 18446744073709551615", Comparison::Lte(UInt(u64::MAX))),
            ("contains: \"a${b}\"", Comparison::Contains(tpl("a${b}"))),
        ]
    }

    #[test]
    fn comparisons_map_to_their_variant() {
        for (row, op) in rows() {
            let json = only_request(&req(&format!(
                "expect: {{json: [{{path: \"$.a\", {row}}}]}}"
            )));
            assert_eq!(
                json.expect.json,
                [JsonAssertion {
                    path: "$.a".parse().unwrap(),
                    op: op.clone()
                }],
                "{row}"
            );
            let yaml = format!("[{{if: {{when: {{var: x, {row}}}, then: []}}}}]");
            let Step::If(step) = ok(&yaml).remove(0) else {
                panic!("not an if")
            };
            assert_eq!(
                step.when,
                Condition::Compare {
                    var: VarRef::Vu("x".into()),
                    op
                },
                "{row}"
            );
        }
    }

    fn when(yaml: &str) -> Condition {
        let full = format!("[{{if: {{when: {yaml}, then: []}}}}]");
        let Step::If(step) = ok(&full).remove(0) else {
            panic!("not an if")
        };
        step.when
    }

    #[test]
    fn conditions_map_to_their_form() {
        let a = Condition::Compare {
            var: VarRef::Vu("token".into()),
            op: Comparison::Exists(true),
        };
        let b = Condition::Compare {
            var: VarRef::Scenario("x".into()),
            op: Comparison::Equals(S::Number(Int(1))),
        };
        assert_eq!(when("{var: token, exists: true}"), a);
        assert_eq!(when("{var: scenario.x, equals: 1}"), b);
        let both = "[{var: token, exists: true}, {var: scenario.x, equals: 1}]";
        assert_eq!(
            when(&format!("{{all: {both}}}")),
            Condition::All(vec![a.clone(), b.clone()])
        );
        assert_eq!(
            when("{any: [{var: token, exists: true}]}"),
            Condition::Any(vec![a.clone()])
        );
        assert_eq!(
            when("{not: {var: token, exists: true}}"),
            Condition::Not(Box::new(a))
        );
        assert_eq!(when("{any: []}"), Condition::Any(vec![]));
    }

    #[test]
    fn if_and_repeat_map_their_keys() {
        let c = "{var: x, exists: true}";
        let cond = Condition::Compare {
            var: VarRef::Vu("x".into()),
            op: Comparison::Exists(true),
        };
        let yaml = format!("[{{if: {{when: {c}, then: [{{wait: 1s}}], else: [{{wait: 2s}}]}}}}]");
        assert_eq!(
            ok(&yaml),
            [Step::If(IfStep {
                when: cond.clone(),
                then: vec![Step::Wait(Duration::from_secs(1))],
                otherwise: vec![Step::Wait(Duration::from_secs(2))],
            })]
        );
        let yaml = format!("[{{if: {{when: {c}, then: []}}}}]");
        assert_eq!(
            ok(&yaml),
            [Step::If(IfStep {
                when: cond,
                then: vec![],
                otherwise: vec![],
            })]
        );
        assert_eq!(
            ok("[{repeat: {count: 2, steps: [{wait: 1s}]}}]"),
            [Step::Repeat(RepeatStep {
                count: NonZeroU32::new(2).unwrap(),
                deadline: DEFAULT_REPEAT_DEADLINE,
                steps: vec![Step::Wait(Duration::from_secs(1))],
            })]
        );
        assert_eq!(
            ok("[{repeat: {count: 2, deadline: 5s, steps: []}}]"),
            [Step::Repeat(RepeatStep {
                count: NonZeroU32::new(2).unwrap(),
                deadline: Duration::from_secs(5),
                steps: vec![],
            })]
        );
    }

    #[test]
    fn nested_requests_inherit_the_timeout() {
        let yaml = "[{if: {when: {var: x, exists: true}, then: [{repeat: {count: 1, steps: [{request: {method: GET, path: /a}}]}}]}}]";
        let Step::If(step) = ok(yaml).remove(0) else {
            panic!()
        };
        let Step::Repeat(rep) = &step.then[0] else {
            panic!()
        };
        let Step::Request(r) = &rep.steps[0] else {
            panic!()
        };
        assert_eq!(r.timeout, Duration::from_secs(7));
    }

    #[test]
    fn a_request_reports_all_its_problems_together() {
        let (_, errors) = run(
            "[{request: {method: get, timeout: x, headers: {A: 1}}}]",
            |node, errors| steps(node, "steps", Duration::from_secs(7), errors),
        );
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(errors.len(), 4, "{errors:?}");
        for p in [
            "steps[0].request.method",
            "steps[0].request",
            "steps[0].request.timeout",
            "steps[0].request.headers.A",
        ] {
            assert!(paths.contains(&p), "{p} missing from {paths:?}");
        }
    }

    #[test]
    fn step_failures_name_their_path() {
        for (yaml, path, fragment) in [
            ("5", "steps", "expected a list"),
            ("[5]", "steps[0]", "expected a map"),
            ("[{}]", "steps[0]", "exactly one key"),
            ("[{wait: 1s, if: {}}]", "steps[0]", "exactly one key"),
            ("[{reqest: {}}]", "steps[0].reqest", "unknown key `reqest`"),
            ("[{wait: 5}]", "steps[0].wait", "expected a duration"),
            ("[{wait: soon}]", "steps[0].wait", "expected a duration"),
            (
                "[{if: {then: [{wait: 1s}]}}]",
                "steps[0].if",
                "missing required key `when`",
            ),
            (
                "[{if: {when: {var: x, exists: true}}}]",
                "steps[0].if",
                "missing required key `then`",
            ),
            (
                "[{if: {when: {}, then: []}}]",
                "steps[0].if.when",
                "needs one of `var`, `all`, `any` or `not`",
            ),
            (
                "[{if: {when: {var: x}, then: []}}]",
                "steps[0].if.when",
                "needs one of",
            ),
            (
                "[{if: {when: {all: [], any: []}, then: []}}]",
                "steps[0].if.when.any",
                "`all` and `any` can't be used together",
            ),
            (
                "[{if: {when: {all: [], exists: true}, then: []}}]",
                "steps[0].if.when.exists",
                "unknown key `exists`",
            ),
            (
                "[{if: {when: {all: [], var: a, exists: true}, then: []}}]",
                "steps[0].if.when.var",
                "`all` and `var` can't be used together",
            ),
            (
                "[{if: {when: {var: \"${x}\", exists: true}, then: []}}]",
                "steps[0].if.when.var",
                "expected a variable reference",
            ),
            (
                "[{if: {when: {not: 5}, then: []}}]",
                "steps[0].if.when.not",
                "expected a map",
            ),
            (
                "[{if: {when: {all: 5}, then: []}}]",
                "steps[0].if.when.all",
                "expected a list",
            ),
            (
                "[{if: {when: {all: [5]}, then: []}}]",
                "steps[0].if.when.all[0]",
                "expected a map",
            ),
            (
                "[{if: {when: {var: x, exists: true}, then: [], other: 1}}]",
                "steps[0].if.other",
                "unknown key `other`",
            ),
            (
                "[{repeat: {steps: []}}]",
                "steps[0].repeat",
                "missing required key `count`",
            ),
            (
                "[{repeat: {count: 0, steps: []}}]",
                "steps[0].repeat.count",
                "expected an integer from 1 to 4294967295",
            ),
            (
                "[{repeat: {count: 1}}]",
                "steps[0].repeat",
                "missing required key `steps`",
            ),
            (
                "[{repeat: {count: 1, deadline: x, steps: []}}]",
                "steps[0].repeat.deadline",
                "expected a duration",
            ),
        ] {
            fails(yaml, path, fragment);
        }
    }

    #[test]
    fn request_failures_name_their_path() {
        const R: &str = "steps[0].request";
        let cases: Vec<(String, String, &str)> = vec![
            ("[{request: 5}]".into(), R.into(), "expected a map"),
            (
                "[{request: {path: /a}}]".into(),
                R.into(),
                "missing required key `method`",
            ),
            (
                "[{request: {method: get, path: /a}}]".into(),
                format!("{R}.method"),
                "expected an HTTP method",
            ),
            (
                "[{request: {method: GET}}]".into(),
                R.into(),
                "needs one of `path` or `url`",
            ),
            (
                "[{request: {method: GET, path: /a, url: /b}}]".into(),
                format!("{R}.url"),
                "`path` and `url` can't be used together",
            ),
            (
                "[{request: {method: GET, path: \"${\"}}]".into(),
                format!("{R}.path"),
                "expected a closing }",
            ),
            (
                "[{request: {method: GET, path: 5}}]".into(),
                format!("{R}.path"),
                "expected a string",
            ),
        ];
        for (yaml, path, fragment) in cases {
            fails(&yaml, &path, fragment);
        }
        for (fields, sub, fragment) in [
            ("name: 5", "name", "expected a string"),
            (
                "json: 1, body: x",
                "body",
                "`json` and `body` can't be used together",
            ),
            (
                "body: x, form: {}",
                "form",
                "`body` and `form` can't be used together",
            ),
            ("body: 5", "body", "expected a string"),
            ("form: 5", "form", "expected a map"),
            ("form: {a: true}", "form.a", "expected a string"),
            ("headers: 5", "headers", "expected a map"),
            ("headers: {A: 1}", "headers.A", "expected a string"),
            ("query: {page: 1}", "query.page", "expected a string"),
            ("json: {a: \"${\"}", "json.a", "expected a closing }"),
            ("json: [\"${\"]", "json[0]", "expected a closing }"),
            ("timeout: 5", "timeout", "expected a duration"),
            ("required: no", "required", "expected a boolean"),
            (
                "retry: {backoff: 1s}",
                "retry",
                "missing required key `attempts`",
            ),
            (
                "retry: {attempts: x}",
                "retry.attempts",
                "expected an integer from 0 to 4294967295",
            ),
            (
                "retry: {attempts: 2, backoff: x}",
                "retry.backoff",
                "expected a duration",
            ),
            (
                "retry: {attempts: 2, allow_non_idempotent: 1}",
                "retry.allow_non_idempotent",
                "expected a boolean",
            ),
            (
                "retry: {attempts: 2, nope: 1}",
                "retry.nope",
                "unknown key `nope`",
            ),
            ("expect: 5", "expect", "expected a map"),
            (
                "expect: {statuss: 1}",
                "expect.statuss",
                "unknown key `statuss`",
            ),
            (
                "expect: {status: 70000}",
                "expect.status",
                "expected an integer from 0 to 65535",
            ),
            (
                "expect: {status: [200, x]}",
                "expect.status[1]",
                "expected an integer from 0 to 65535",
            ),
            (
                "expect: {body_contains: 5}",
                "expect.body_contains",
                "expected a string",
            ),
            ("expect: {json: 5}", "expect.json", "expected a list"),
            (
                "expect: {json: [{exists: true}]}",
                "expect.json[0]",
                "missing required key `path`",
            ),
            (
                "expect: {json: [{path: bad, exists: true}]}",
                "expect.json[0].path",
                "expected a JSON path",
            ),
            (
                "expect: {json: [{path: \"$.a\"}]}",
                "expect.json[0]",
                "needs one of",
            ),
            (
                "expect: {json: [{path: \"$.a\", exists: true, gt: 1}]}",
                "expect.json[0].gt",
                "`exists` and `gt` can't be used together",
            ),
            (
                "expect: {json: [{path: \"$.a\", exists: 1}]}",
                "expect.json[0].exists",
                "expected a boolean",
            ),
            (
                "expect: {json: [{path: \"$.a\", equals: [1]}]}",
                "expect.json[0].equals",
                "expected a string, number, boolean or null",
            ),
            (
                "expect: {json: [{path: \"$.a\", gt: x}]}",
                "expect.json[0].gt",
                "expected a number",
            ),
            (
                "expect: {json: [{path: \"$.a\", contains: 5}]}",
                "expect.json[0].contains",
                "expected a string",
            ),
            ("extract: 5", "extract", "expected a map"),
            ("extract: {x: 5}", "extract.x", "expected a map"),
            (
                "extract: {x: {}}",
                "extract.x",
                "needs one of `json_path` or `header`",
            ),
            (
                "extract: {x: {json_path: \"$.a\", header: B}}",
                "extract.x.header",
                "`json_path` and `header` can't be used together",
            ),
            (
                "extract: {x: {json_path: bad}}",
                "extract.x.json_path",
                "expected a JSON path",
            ),
            (
                "extract: {x: {header: 5}}",
                "extract.x.header",
                "expected a string",
            ),
            ("nope: 1", "nope", "unknown key `nope`"),
        ] {
            fails(&req(fields), &format!("{R}.{sub}"), fragment);
        }
    }

    #[test]
    fn unrepresentable_scalars_name_their_kind() {
        let yaml = req("expect: {json: [{path: \"$.a\", equals: {a: 1}}]}");
        fails(
            &yaml,
            "steps[0].request.expect.json[0].equals",
            "found a map",
        );
        let yaml = req("expect: {json: [{path: \"$.a\", equals: [1]}]}");
        fails(
            &yaml,
            "steps[0].request.expect.json[0].equals",
            "found a list",
        );
    }
}
