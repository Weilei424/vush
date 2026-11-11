//! The protocol boundary, used the way an outside crate uses it.

use std::collections::HashSet;
use std::time::Duration;

use vush_core::plan::{
    Expect, HttpMethod, HttpSettings, JsonNumber, RequestStep, RequestTarget, ResourceLimits,
    TlsSettings,
};
use vush_core::protocol::http::{
    HttpBody, HttpHop, HttpRequest, HttpResponse, RequestUrl, WireVersion,
};
use vush_core::protocol::{
    AttemptRecord, BodySizes, ConnectionId, ConnectionSetup, Failure, FailureKind, FailureStage,
    HopRecord, HopTiming, JsonValue, Outcome, StepRecord,
};

const _: () = {
    const fn assert_shareable<T: Send + Sync + 'static>() {}
    assert_shareable::<FailureKind>();
    assert_shareable::<FailureStage>();
    assert_shareable::<Failure>();
    assert_shareable::<HopTiming>();
    assert_shareable::<ConnectionSetup>();
    assert_shareable::<ConnectionId>();
    assert_shareable::<BodySizes>();
    assert_shareable::<Outcome>();
    assert_shareable::<HopRecord>();
    assert_shareable::<AttemptRecord>();
    assert_shareable::<StepRecord>();
    assert_shareable::<JsonValue>();
    assert_shareable::<HttpRequest>();
    assert_shareable::<RequestUrl>();
    assert_shareable::<HttpBody>();
    assert_shareable::<HttpResponse>();
    assert_shareable::<WireVersion>();
    assert_shareable::<HttpHop>();
};

/// Each kind's place in `FailureKind::ALL`. The match has no wildcard, so a
/// new kind won't compile until it is placed here and in `ALL`.
fn kind_position(kind: FailureKind) -> usize {
    match kind {
        FailureKind::InvalidRequest => 0,
        FailureKind::Dns => 1,
        FailureKind::Connect => 2,
        FailureKind::Tls => 3,
        FailureKind::Timeout => 4,
        FailureKind::ConnectionClosed => 5,
        FailureKind::Protocol => 6,
        FailureKind::InvalidRedirect => 7,
        FailureKind::TooManyRedirects => 8,
        FailureKind::Other => 9,
    }
}

fn stage_position(stage: FailureStage) -> usize {
    match stage {
        FailureStage::Prepare => 0,
        FailureStage::Connect => 1,
        FailureStage::Headers => 2,
        FailureStage::Body => 3,
    }
}

#[test]
fn failure_kind_labels_are_pinned() {
    let labels: Vec<&str> = FailureKind::ALL.iter().map(|kind| kind.label()).collect();
    assert_eq!(
        labels,
        [
            "invalid_request",
            "dns",
            "connect",
            "tls",
            "timeout",
            "connection_closed",
            "protocol",
            "invalid_redirect",
            "too_many_redirects",
            "other",
        ]
    );
    for (position, kind) in FailureKind::ALL.into_iter().enumerate() {
        assert_eq!(kind_position(kind), position, "{kind:?}");
    }
}

#[test]
fn failure_stage_labels_are_pinned() {
    let labels: Vec<&str> = FailureStage::ALL
        .iter()
        .map(|stage| stage.label())
        .collect();
    assert_eq!(labels, ["prepare", "connect", "headers", "body"]);
    for (position, stage) in FailureStage::ALL.into_iter().enumerate() {
        assert_eq!(stage_position(stage), position, "{stage:?}");
    }
}

fn assert_snake_case(label: &str) {
    assert!(!label.is_empty(), "{label}");
    assert!(
        !label.starts_with('_') && !label.ends_with('_') && !label.contains("__"),
        "{label}"
    );
    assert!(
        label.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
        "{label}"
    );
}

#[test]
fn labels_are_unique_snake_case_and_match_display() {
    let kinds: Vec<(&str, String)> = FailureKind::ALL
        .iter()
        .map(|k| (k.label(), k.to_string()))
        .collect();
    let stages: Vec<(&str, String)> = FailureStage::ALL
        .iter()
        .map(|s| (s.label(), s.to_string()))
        .collect();
    for list in [&kinds, &stages] {
        for (label, shown) in list {
            assert_eq!(label, shown);
            assert_snake_case(label);
        }
        let unique: HashSet<&str> = list.iter().map(|(label, _)| *label).collect();
        assert_eq!(unique.len(), list.len());
    }
}

#[test]
fn a_failure_displays_as_one_line() {
    let failure = Failure {
        kind: FailureKind::Timeout,
        stage: FailureStage::Body,
        message: "no data in time",
    };
    assert_eq!(failure.to_string(), "timeout in body: no data in time");
    let error: &dyn std::error::Error = &failure;
    assert!(error.source().is_none());
}

fn hop_record(outcome: Outcome) -> HopRecord {
    HopRecord {
        timing: HopTiming {
            to_headers: Some(Duration::from_millis(3)),
            total: Duration::from_millis(5),
            setup: Some(ConnectionSetup {
                dns: None,
                connect: Duration::from_millis(1),
                tls: None,
            }),
        },
        connection: Some(ConnectionId(1)),
        sizes: BodySizes::default(),
        outcome,
    }
}

fn answered(status: u16) -> Outcome {
    Outcome::Response {
        status,
        truncated: false,
    }
}

#[test]
fn records_count_hops_attempts_and_steps_separately() {
    let mut first = AttemptRecord::new(hop_record(answered(302)));
    first.push(hop_record(answered(503)));
    let mut second = AttemptRecord::new(hop_record(answered(302)));
    second.push(hop_record(answered(200)));
    let mut step = StepRecord::new(first);
    step.push(second);

    assert_eq!(step.attempts().len(), 2);
    let hops: usize = step
        .attempts()
        .iter()
        .map(|attempt| attempt.hops().len())
        .sum();
    assert_eq!(hops, 4);
    let outcomes: Vec<Outcome> = step.attempts().iter().map(AttemptRecord::outcome).collect();
    assert_eq!(outcomes, [answered(503), answered(200)]);
}

#[test]
fn new_records_are_never_empty() {
    let attempt = AttemptRecord::new(hop_record(answered(200)));
    assert_eq!(attempt.hops().len(), 1);
    assert_eq!(StepRecord::new(attempt).attempts().len(), 1);
}

#[test]
fn an_ending_after_the_last_hop_is_the_attempts_outcome() {
    for kind in [FailureKind::InvalidRedirect, FailureKind::TooManyRedirects] {
        let mut attempt = AttemptRecord::new(hop_record(answered(302)));
        attempt.end_with(kind, FailureStage::Prepare);
        assert_eq!(attempt.hops().len(), 1);
        assert_eq!(
            attempt.outcome(),
            Outcome::Failed {
                kind,
                stage: FailureStage::Prepare
            }
        );
    }
}

#[test]
fn without_an_ending_an_attempt_reports_its_last_hop() {
    let refused = Outcome::Failed {
        kind: FailureKind::Connect,
        stage: FailureStage::Connect,
    };
    let mut attempt = AttemptRecord::new(hop_record(answered(302)));
    attempt.push(hop_record(refused));
    assert_eq!(attempt.outcome(), refused);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic]
fn pushing_after_an_ending_is_a_bug() {
    let mut attempt = AttemptRecord::new(hop_record(answered(302)));
    attempt.end_with(FailureKind::TooManyRedirects, FailureStage::Prepare);
    attempt.push(hop_record(answered(200)));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic]
fn ending_twice_is_a_bug() {
    let mut attempt = AttemptRecord::new(hop_record(answered(302)));
    attempt.end_with(FailureKind::InvalidRedirect, FailureStage::Prepare);
    attempt.end_with(FailureKind::TooManyRedirects, FailureStage::Prepare);
}

const SECRET: &str = "hunter2";

fn get() -> HttpMethod {
    "GET".parse().expect("GET is a method")
}

/// No planted secret or planted number shows in `debug`.
fn assert_hidden(debug: &str) {
    assert!(!debug.contains(SECRET), "{debug}");
    assert!(!debug.contains("271828"), "{debug}");
}

fn secret_json() -> JsonValue {
    JsonValue::Object(vec![
        (
            "user".to_string(),
            JsonValue::Array(vec![JsonValue::String(SECRET.to_string())]),
        ),
        (
            "pin".to_string(),
            JsonValue::Number(JsonNumber::Int(271_828)),
        ),
        ("ok".to_string(), JsonValue::Bool(true)),
    ])
}

fn secret_request(url: RequestUrl, body: HttpBody) -> HttpRequest {
    HttpRequest {
        method: get(),
        url,
        query: vec![("page".to_string(), SECRET.to_string())],
        headers: vec![("authorization".to_string(), format!("Bearer {SECRET}"))],
        body: Some(body),
    }
}

#[test]
fn request_debug_hides_every_value() {
    let urls = [
        RequestUrl::Absolute(format!(
            "https://user:{SECRET}@example.test/a?token={SECRET}"
        )),
        RequestUrl::Joined {
            base: format!("https://{SECRET}.example.test"),
            path: format!("/{SECRET}"),
        },
    ];
    let bodies = [
        HttpBody::Json(secret_json()),
        HttpBody::Text(SECRET.to_string()),
        HttpBody::Form(vec![("password".to_string(), SECRET.to_string())]),
    ];
    for url in &urls {
        for body in &bodies {
            let debug = format!("{:?}", secret_request(url.clone(), body.clone()));
            assert_hidden(&debug);
            for shown in ["GET", "authorization", "page"] {
                assert!(debug.contains(shown), "{debug}");
            }
        }
    }
    let json = format!("{:?}", secret_json());
    assert_hidden(&json);
    for shown in ["user", "pin", "true"] {
        assert!(json.contains(shown), "{json}");
    }
    let form = format!("{:?}", bodies[2]);
    assert_hidden(&form);
    assert!(form.contains("password"), "{form}");
}

#[test]
fn debug_escapes_control_characters_in_names() {
    let name = "x-a\r\nInjected: yes".to_string();
    let request = HttpRequest {
        method: get(),
        url: RequestUrl::Absolute("http://example.test/".to_string()),
        query: vec![(name.clone(), String::new())],
        headers: vec![(name.clone(), String::new())],
        body: Some(HttpBody::Form(vec![(name.clone(), String::new())])),
    };
    let json = JsonValue::Object(vec![(name, JsonValue::Null)]);
    for debug in [format!("{request:?}"), format!("{json:?}")] {
        assert!(!debug.chars().any(char::is_control), "{debug}");
        assert!(debug.contains(r"\r\n"), "{debug}");
    }
}

#[test]
fn debug_counts_multibyte_text_without_slicing() {
    let request = HttpRequest {
        method: get(),
        url: RequestUrl::Joined {
            base: "http://日本.test".to_string(),
            path: "/é".to_string(),
        },
        query: Vec::new(),
        headers: Vec::new(),
        body: Some(HttpBody::Text("héllo".to_string())),
    };
    let debug = format!("{request:?}");
    assert!(
        !debug.contains("héllo") && !debug.contains("日本"),
        "{debug}"
    );
    // "héllo" is 6 bytes. The URL's parts are 18 and 3 bytes.
    assert!(debug.contains('6'), "{debug}");
}

/// The nesting `WorkloadPlan::new` documents as tested on a 2 MiB stack.
const TESTED_NESTING: usize = 1_000;

#[test]
fn json_nested_to_the_tested_bound_works_on_a_small_stack() {
    let (length, leaked) = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let mut value = JsonValue::String(SECRET.to_string());
            for _ in 0..TESTED_NESTING {
                value = JsonValue::Array(vec![value]);
            }
            let copy = value.clone();
            assert_eq!(copy, value);
            let debug = format!("{value:?}");
            (debug.len(), debug.contains(SECRET))
        })
        .expect("the thread starts")
        .join()
        .expect("no stack overflow");
    assert!(length > TESTED_NESTING);
    assert!(!leaked);
}

#[test]
fn every_plan_field_has_an_owner() {
    let step = RequestStep {
        name: None,
        method: get(),
        target: RequestTarget::Url("http://example.test/".parse().expect("a template")),
        query: Vec::new(),
        headers: Vec::new(),
        body: None,
        timeout: Duration::from_secs(1),
        required: true,
        retry: None,
        expect: Expect::default(),
        extract: Vec::new(),
    };
    // No `..` below: a new plan field won't compile until it has an owner here.
    let RequestStep {
        name: _,     // runner and metrics (Phases 3, 6): stays in core as a label
        method: _,   // request: HttpRequest.method
        target: _,   // request: `path` is RequestUrl::Joined, `url` is RequestUrl::Absolute
        query: _,    // request: HttpRequest.query, resolved
        headers: _,  // request: HttpRequest.headers, resolved
        body: _,     // request: HttpBody::Json, Text or Form
        timeout: _,  // driver (P2.2, P2.3): the attempt's limit; each hop gets what is left
        required: _, // runner (Phase 3)
        retry: _,    // driver (P2.3): the attempt loop
        expect: _,   // runner (Phase 3): checked on the last hop's response
        extract: _,  // runner (Phase 3): read from the last hop's response
    } = step;
    let HttpSettings {
        version: _,           // executor construction (P2.3)
        pool: _,              // executor construction (P2.3); `vu` puts a pool in each session
        timeout: _,           // vush-config: already copied into each request step's `timeout`
        redirects: _,         // driver (P2.2): the hop loop's limit
        cookies: _,           // session (P2.4)
        max_response_size: _, // executor construction (P2.5): caps the body, sets `truncated`
        tls:
            TlsSettings {
                ca_file: _,  // executor construction (P2.3)
                insecure: _, // executor construction (P2.3)
            },
    } = HttpSettings::default();
    let ResourceLimits {
        max_variables: _,     // runner (Phase 3)
        max_variable_size: _, // runner (Phase 3)
        max_cookies: _,       // session (P2.4)
        hook_timeout: _,      // runner (Phase 3)
    } = ResourceLimits::default();
}

fn http_response(status: u16) -> HttpResponse {
    HttpResponse {
        status,
        version: WireVersion::Http11,
        headers: vec![("content-type".to_string(), b"text/plain".to_vec())],
        body: b"ok".to_vec(),
        truncated: false,
    }
}

fn hop_ok(status: u16) -> HttpHop {
    HttpHop {
        timing: HopTiming {
            to_headers: Some(Duration::from_millis(4)),
            total: Duration::from_millis(9),
            setup: None,
        },
        connection: Some(ConnectionId(7)),
        sizes: BodySizes {
            sent: 0,
            received: 2,
        },
        outcome: Ok(http_response(status)),
    }
}

fn hop_failed(kind: FailureKind, stage: FailureStage, message: &'static str) -> HttpHop {
    HttpHop {
        timing: HopTiming {
            to_headers: None,
            total: Duration::from_millis(9),
            setup: None,
        },
        connection: None,
        sizes: BodySizes::default(),
        outcome: Err(Failure {
            kind,
            stage,
            message,
        }),
    }
}

#[test]
fn a_5xx_is_a_response_and_a_refused_connection_is_a_failure() {
    let refused = hop_failed(
        FailureKind::Connect,
        FailureStage::Connect,
        "connection refused",
    );
    let cases = [
        (hop_ok(200), answered(200)),
        (hop_ok(503), answered(503)),
        (
            refused,
            Outcome::Failed {
                kind: FailureKind::Connect,
                stage: FailureStage::Connect,
            },
        ),
    ];
    for (hop, outcome) in cases {
        let want = HopRecord {
            timing: hop.timing,
            connection: hop.connection,
            sizes: hop.sizes,
            outcome,
        };
        assert_eq!(hop.record(), want);
    }
}

#[test]
fn a_truncated_body_is_still_a_response() {
    let mut hop = hop_ok(200);
    if let Ok(response) = &mut hop.outcome {
        response.truncated = true;
    }
    assert_eq!(
        hop.record().outcome,
        Outcome::Response {
            status: 200,
            truncated: true
        }
    );
}

#[test]
fn response_and_hop_debug_hide_every_value() {
    let body = format!("{{\"token\": \"{SECRET}\", \"n\": 271828}}").into_bytes();
    let length = body.len().to_string();
    let response = HttpResponse {
        status: 207,
        version: WireVersion::Http2,
        headers: vec![(
            "set-cookie".to_string(),
            format!("sid={SECRET}").into_bytes(),
        )],
        body,
        truncated: false,
    };
    let debug = format!("{response:?}");
    assert_hidden(&debug);
    for shown in ["207", "set-cookie", length.as_str()] {
        assert!(debug.contains(shown), "{debug}");
    }
    let hop = HttpHop {
        outcome: Ok(response),
        ..hop_ok(207)
    };
    assert_hidden(&format!("{hop:?}"));
    let failed = hop_failed(
        FailureKind::Tls,
        FailureStage::Connect,
        "certificate not trusted",
    );
    assert_hidden(&format!("{failed:?}"));
    if let Err(failure) = &failed.outcome {
        assert_hidden(&failure.to_string());
    }
}

#[test]
fn response_debug_handles_raw_header_bytes_and_empty_bodies() {
    let response = HttpResponse {
        status: 204,
        version: WireVersion::Http10,
        headers: vec![("x-bin".to_string(), vec![0xff, 0xfe])],
        body: Vec::new(),
        truncated: false,
    };
    let debug = format!("{response:?}");
    assert!(debug.contains("x-bin") && debug.contains("204"), "{debug}");
}
