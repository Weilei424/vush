//! The canary: a secret marker rides in every part of a request, every
//! failure path runs, and the marker must not show in a failure text, a
//! `Debug` output or a record.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use rustls::RootCertStore;
use vush_core::plan::{HttpSettings, HttpVersion, ResourceLimits, RetryPolicy};
use vush_core::protocol::http::{HttpBody, HttpExecutor, HttpHop, HttpRequest, RequestUrl};
use vush_core::protocol::{FailureKind, FailureStage, JsonValue, Outcome};
use vush_core::runner::{StepPolicy, Timer, run_attempt, run_step};
use vush_http::Executor;

use crate::common::{
    LOCALHOST, MIB, NO_DIR, TIMEOUT, TestCa, close_after_head, echo, executor, failure,
    garbage_status, get, hundred_and_one_headers, limited, one_503_then_gone, pairs, redirecting,
    refused_port, reply, send, send_within, settings, silent_after_head, spawn_h1, spawn_raw,
    spawn_tls, stalled_body, trusting,
};

const CANARY: &str = "vush-canary-7f3a";

fn assert_clean_text(text: &str) {
    assert!(!text.contains(CANARY), "the canary leaked into {text}");
}

fn assert_clean(hop: &HttpHop) {
    assert_clean_text(&format!("{hop:?}"));
    assert_clean_text(&format!("{:?}", hop.record()));
    if let Err(failure) = &hop.outcome {
        assert_clean_text(&format!("{failure}"));
        assert_clean_text(&format!("{failure:?}"));
    }
}

/// Puts the canary in the query, `Authorization`, `Cookie` and body of
/// `request`.
fn with_canary(mut request: HttpRequest, body: Option<HttpBody>) -> HttpRequest {
    request.query = pairs(&[("token", CANARY)]);
    request.headers = pairs(&[
        ("Authorization", &format!("Bearer {CANARY}")),
        ("Cookie", &format!("session={CANARY}")),
    ]);
    request.body = body;
    request
}

/// One request per body kind, each carrying the canary. The URL carries it
/// in its path.
fn requests(base: &str) -> Vec<HttpRequest> {
    let url = format!("{base}/{CANARY}");
    let bodies = [
        HttpBody::Text(CANARY.to_owned()),
        HttpBody::Form(pairs(&[("secret", CANARY)])),
        HttpBody::Json(JsonValue::Object(vec![(
            "secret".to_owned(),
            JsonValue::String(CANARY.to_owned()),
        )])),
    ];
    let mut all = vec![with_canary(get(url.clone()), None)];
    for body in bodies {
        let mut request = with_canary(get(url.clone()), Some(body));
        request.method = "POST".parse().unwrap();
        all.push(request);
    }
    all
}

/// Sends each request of [`requests`], and checks the hop failed as
/// `expected` and shows no canary.
async fn expect_failure(
    executor: &Executor,
    base: &str,
    timeout: Duration,
    expected: (FailureKind, FailureStage),
) {
    for request in requests(base) {
        assert_clean_text(&format!("{request:?}"));
        let hop = send_within(executor, &request, timeout).await;
        assert_eq!(failure(&hop), expected);
        assert_clean(&hop);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_requests_never_show_the_canary() {
    let executor = executor(HttpVersion::Auto);
    let invalid = (FailureKind::InvalidRequest, FailureStage::Prepare);

    let mut control = with_canary(get(format!("http://127.0.0.1:1/{CANARY}")), None);
    control
        .headers
        .push(("X-Bad".into(), format!("{CANARY}\r\nb")));
    assert_clean_text(&format!("{control:?}"));
    let hop = send(&executor, &control).await;
    assert_eq!(failure(&hop), invalid);
    assert_clean(&hop);

    for base in [
        format!("http://user:{CANARY}@127.0.0.1:1"),
        format!("http://[{CANARY}"),
        format!("http://{CANARY} space"),
    ] {
        expect_failure(&executor, &base, TIMEOUT, invalid).await;
    }

    let mut joined = get(String::new());
    joined.url = RequestUrl::Absolute(format!("{CANARY}/no-scheme"));
    let joined = with_canary(joined, None);
    assert_clean_text(&format!("{joined:?}"));
    let hop = send(&executor, &joined).await;
    assert_eq!(failure(&hop), invalid);
    assert_clean(&hop);
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_and_tls_failures_never_show_the_canary() {
    let refused = refused_port();
    expect_failure(
        &executor(HttpVersion::Auto),
        &format!("http://{}", refused.addr),
        TIMEOUT,
        (FailureKind::Connect, FailureStage::Connect),
    )
    .await;

    let ca = TestCa::generate();
    let untrusted = spawn_tls(echo, &[b"http/1.1"], None, ca.leaf_for_ip(LOCALHOST)).await;
    expect_failure(
        &trusting(&TestCa::generate(), HttpVersion::Auto),
        &format!("https://{untrusted}"),
        TIMEOUT,
        (FailureKind::Tls, FailureStage::Connect),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn timeouts_never_show_the_canary() {
    let short = Duration::from_millis(300);
    let silent = spawn_raw(silent_after_head).await;
    expect_failure(
        &executor(HttpVersion::Auto),
        &format!("http://{silent}"),
        short,
        (FailureKind::Timeout, FailureStage::Headers),
    )
    .await;

    let stalled = spawn_raw(stalled_body).await;
    expect_failure(
        &executor(HttpVersion::Auto),
        &format!("http://{stalled}"),
        short,
        (FailureKind::Timeout, FailureStage::Body),
    )
    .await;
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn closed_and_broken_responses_never_show_the_canary() {
    let closed = spawn_raw(close_after_head).await;
    expect_failure(
        &executor(HttpVersion::Auto),
        &format!("http://{closed}"),
        TIMEOUT,
        (FailureKind::ConnectionClosed, FailureStage::Headers),
    )
    .await;

    let garbage = spawn_raw(garbage_status).await;
    expect_failure(
        &executor(HttpVersion::Auto),
        &format!("http://{garbage}"),
        TIMEOUT,
        (FailureKind::Protocol, FailureStage::Headers),
    )
    .await;

    let many = spawn_raw(hundred_and_one_headers).await;
    expect_failure(
        &executor(HttpVersion::Auto),
        &format!("http://{many}"),
        TIMEOUT,
        (FailureKind::Protocol, FailureStage::Headers),
    )
    .await;

    // The server's bytes are the canary, labelled gzip, so they are corrupt.
    let corrupt = spawn_h1(|_| {
        std::future::ready(reply(
            200,
            &[("content-encoding", "gzip")],
            CANARY.repeat(4),
        ))
    })
    .await;
    let settings = HttpSettings {
        decompress: true,
        ..settings(HttpVersion::Http1, MIB)
    };
    let decoding = Executor::with_roots(
        &settings,
        &ResourceLimits::default(),
        Path::new(NO_DIR),
        RootCertStore::empty(),
    )
    .unwrap();
    expect_failure(
        &decoding,
        &format!("http://{corrupt}"),
        TIMEOUT,
        (FailureKind::Protocol, FailureStage::Body),
    )
    .await;

    // A valid gzip body holding the canary is decoded, and still never shown.
    let valid = spawn_h1(|_| {
        std::future::ready(reply(
            200,
            &[("content-encoding", "gzip")],
            gzip(CANARY.as_bytes()),
        ))
    })
    .await;
    let hop = send(&decoding, &get(format!("http://{valid}/"))).await;
    assert_clean(&hop);
    let got = crate::common::response(&hop);
    assert_eq!(got.status, 200);
    assert_eq!(got.body, CANARY.as_bytes());
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_failures_never_show_the_canary() {
    let addr = spawn_h1(redirecting(&[
        ("/ftp", 302, &format!("ftp://{CANARY}/")),
        ("/a", 302, &format!("/b?token={CANARY}")),
        ("/b", 302, &format!("/c?token={CANARY}")),
    ]))
    .await;
    let executor = executor(HttpVersion::Auto);

    let cases = [
        ("/ftp", 1, FailureKind::InvalidRedirect),
        ("/a", 1, FailureKind::TooManyRedirects),
    ];
    for (path, redirects, kind) in cases {
        let mut session = executor.new_session();
        let request = with_canary(
            get(format!("http://{addr}{path}")),
            Some(HttpBody::Text(CANARY.to_owned())),
        );
        let result = run_attempt(&executor, &mut session, request, TIMEOUT, redirects).await;
        assert_eq!(
            result.record.outcome(),
            Outcome::Failed {
                kind,
                stage: FailureStage::Prepare
            }
        );
        assert_clean_text(&format!("{:?}", result.record));
        assert_clean_text(&format!("{:?}", result.record.outcome()));
        for hop in result.record.hops() {
            assert_clean_text(&format!("{hop:?}"));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_truncated_response_never_shows_the_canary() {
    let addr = spawn_h1(|_| {
        std::future::ready(reply(
            200,
            &[("set-cookie", &format!("s={CANARY}")), ("x-secret", CANARY)],
            CANARY.repeat(8),
        ))
    })
    .await;
    let executor = limited(HttpVersion::Auto, 16);
    for request in requests(&format!("http://{addr}")) {
        let hop = send(&executor, &request).await;
        let got = crate::common::response(&hop);
        assert!(got.truncated);
        assert_eq!(got.body.len(), 16);
        assert_clean(&hop);
    }
}

struct TokioSleep;

impl Timer for TokioSleep {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn retried_steps_never_show_the_canary() {
    let addr = one_503_then_gone().await;
    let executor = executor(HttpVersion::Http1);
    let mut session = executor.new_session();
    let request = with_canary(
        get(format!("http://{addr}/{CANARY}")),
        Some(HttpBody::Text(CANARY.to_owned())),
    );
    let policy = StepPolicy {
        timeout: TIMEOUT,
        redirects: 0,
        retry: Some(RetryPolicy {
            attempts: 2,
            backoff: Duration::ZERO,
            allow_non_idempotent: true,
        }),
    };
    let result = run_step(&executor, &TokioSleep, &mut session, request, &policy).await;

    let attempts = result.record.attempts();
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts[0].outcome(),
        Outcome::Response {
            status: 503,
            truncated: false
        }
    );
    assert_eq!(
        attempts[1].outcome(),
        Outcome::Failed {
            kind: FailureKind::Connect,
            stage: FailureStage::Connect
        }
    );
    assert_clean_text(&format!("{:?}", result.record));
    for attempt in attempts {
        assert_clean_text(&format!("{attempt:?}"));
    }
}
