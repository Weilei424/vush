//! `run_step`'s retry loop over the real executor: a `5xx` answer is tried
//! again on the same session, and so is a refused connection.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use vush_core::plan::{HttpVersion, RetryPolicy};
use vush_core::protocol::http::HttpExecutor;
use vush_core::protocol::{FailureKind, FailureStage, Outcome};
use vush_core::runner::{StepPolicy, Timer, run_step};

use crate::common::{
    Echo, ReplyFuture, TIMEOUT, TestCa, echo, executor, get, refused_port, reply, spawn_gated_h1,
    spawn_h1, trusting,
};

/// The real clock for `run_step`'s backoff.
struct TokioSleep;

impl Timer for TokioSleep {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

fn policy(backoff: Duration) -> StepPolicy {
    StepPolicy {
        timeout: TIMEOUT,
        redirects: 0,
        retry: Some(RetryPolicy {
            attempts: 2,
            backoff,
            allow_non_idempotent: false,
        }),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_503_then_a_200_takes_two_attempts_on_one_session() {
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    let ca = TestCa::generate();
    let addr = spawn_gated_h1(
        move |_request| {
            let status = if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                503
            } else {
                200
            };
            std::future::ready(reply(status, &[], "x"))
        },
        &ca,
        1,
    )
    .await;
    let executor = trusting(&ca, HttpVersion::Auto);
    let mut session = executor.new_session();

    let result = run_step(
        &executor,
        &TokioSleep,
        &mut session,
        get(format!("https://{addr}/")),
        &policy(Duration::from_millis(10)),
    )
    .await;

    let attempts = result.record.attempts();
    assert_eq!(attempts.len(), 2);
    let statuses: Vec<u16> = attempts
        .iter()
        .map(|attempt| match attempt.outcome() {
            Outcome::Response { status, .. } => status,
            other => panic!("expected a response, got {other:?}"),
        })
        .collect();
    assert_eq!(statuses, [503, 200]);
    assert_eq!(result.response.unwrap().status, 200);
    let first = attempts[0].hops()[0].connection;
    assert!(first.is_some());
    assert_eq!(first, attempts[1].hops()[0].connection);
    assert_eq!(seen.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_port_is_tried_twice() {
    let refused = refused_port();
    let executor = executor(HttpVersion::Auto);
    let mut session = executor.new_session();

    let result = run_step(
        &executor,
        &TokioSleep,
        &mut session,
        get(format!("http://{}/", refused.addr)),
        &policy(Duration::ZERO),
    )
    .await;

    let attempts = result.record.attempts();
    assert_eq!(attempts.len(), 2);
    for attempt in attempts {
        assert_eq!(
            attempt.outcome(),
            Outcome::Failed {
                kind: FailureKind::Connect,
                stage: FailureStage::Connect,
            }
        );
    }
    assert!(result.response.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_sees_cookies_from_the_failed_attempt() {
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    let addr = spawn_h1(move |request| -> ReplyFuture {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            Box::pin(std::future::ready(reply(
                503,
                &[("set-cookie", "tried=1")],
                "",
            )))
        } else {
            echo(request)
        }
    })
    .await;
    let executor = executor(HttpVersion::Auto);
    let mut session = executor.new_session();

    let result = run_step(
        &executor,
        &TokioSleep,
        &mut session,
        get(format!("http://{addr}/")),
        &policy(Duration::ZERO),
    )
    .await;

    assert_eq!(result.record.attempts().len(), 2);
    let echoed = Echo::of_response(result.response.as_ref().expect("a response"));
    assert_eq!(echoed.values("cookie"), ["tried=1"]);
}
