//! The attempt loop, driven by a scripted executor.

use std::collections::VecDeque;
use std::pin::pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use vush_core::plan::RetryPolicy;
use vush_core::protocol::http::{
    HttpExecutor, HttpHop, HttpRequest, HttpResponse, RequestUrl, WireVersion,
};
use vush_core::protocol::{BodySizes, Failure, FailureKind, FailureStage, HopTiming, Outcome};
use vush_core::runner::{StepPolicy, StepResult, Timer, run_attempt, run_step};

/// Polls a future once. The fake never waits, so one poll finishes it.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the fake executor never waits"),
    }
}

fn assert_send<T: Send>(_: &T) {}

struct ScriptedExecutor {
    hops: Mutex<VecDeque<HttpHop>>,
    follows: Mutex<VecDeque<Result<Option<HttpRequest>, Failure>>>,
    timeouts: Mutex<Vec<Duration>>,
    urls: Mutex<Vec<RequestUrl>>,
    follow_calls: AtomicUsize,
}

impl ScriptedExecutor {
    fn new(hops: Vec<HttpHop>, follows: Vec<Result<Option<HttpRequest>, Failure>>) -> Self {
        Self {
            hops: Mutex::new(hops.into()),
            follows: Mutex::new(follows.into()),
            timeouts: Mutex::new(Vec::new()),
            urls: Mutex::new(Vec::new()),
            follow_calls: AtomicUsize::new(0),
        }
    }

    fn timeouts(&self) -> Vec<Duration> {
        self.timeouts.lock().unwrap().clone()
    }

    fn urls(&self) -> Vec<RequestUrl> {
        self.urls.lock().unwrap().clone()
    }

    fn follow_calls(&self) -> usize {
        self.follow_calls.load(Ordering::SeqCst)
    }
}

impl HttpExecutor for ScriptedExecutor {
    type Session = ();

    fn new_session(&self) {}

    async fn send(&self, _session: &mut (), request: &HttpRequest, timeout: Duration) -> HttpHop {
        self.urls.lock().unwrap().push(request.url.clone());
        self.timeouts.lock().unwrap().push(timeout);
        self.hops
            .lock()
            .unwrap()
            .pop_front()
            .expect("a hop is scripted for every send")
    }

    fn follow(
        &self,
        _request: &HttpRequest,
        _response: &HttpResponse,
    ) -> Result<Option<HttpRequest>, Failure> {
        self.follow_calls.fetch_add(1, Ordering::SeqCst);
        self.follows
            .lock()
            .unwrap()
            .pop_front()
            .expect("an answer is scripted for every follow")
    }
}

fn timing(total_secs: u64) -> HopTiming {
    HopTiming {
        to_headers: None,
        total: Duration::from_secs(total_secs),
        setup: None,
    }
}

fn answered(status: u16, total_secs: u64) -> HttpHop {
    HttpHop {
        timing: timing(total_secs),
        connection: None,
        sizes: BodySizes::default(),
        outcome: Ok(HttpResponse {
            status,
            version: WireVersion::Http11,
            headers: Vec::new(),
            body: Vec::new(),
            truncated: false,
        }),
    }
}

fn failure(kind: FailureKind, stage: FailureStage) -> Failure {
    Failure {
        kind,
        stage,
        message: "scripted",
    }
}

fn failed(kind: FailureKind, stage: FailureStage, total_secs: u64) -> HttpHop {
    HttpHop {
        timing: timing(total_secs),
        connection: None,
        sizes: BodySizes::default(),
        outcome: Err(failure(kind, stage)),
    }
}

fn request() -> HttpRequest {
    HttpRequest {
        method: "GET".parse().expect("GET is a method"),
        url: RequestUrl::Absolute("http://127.0.0.1/".to_string()),
        query: Vec::new(),
        headers: Vec::new(),
        body: None,
    }
}

const THIRTY: Duration = Duration::from_secs(30);

fn next() -> Result<Option<HttpRequest>, Failure> {
    Ok(Some(request()))
}

#[test]
fn one_hop_without_a_redirect_returns_its_response() {
    let fake = ScriptedExecutor::new(vec![answered(200, 1)], vec![Ok(None)]);
    let mut session = ();
    let future = run_attempt(&fake, &mut session, request(), THIRTY, 5);
    assert_send(&future);
    let result = block_on(future);
    assert_eq!(result.record.hops().len(), 1);
    assert_eq!(
        result.record.outcome(),
        Outcome::Response {
            status: 200,
            truncated: false
        }
    );
    assert_eq!(result.response.unwrap().status, 200);
}

#[test]
fn a_followed_chain_records_every_hop() {
    let fake = ScriptedExecutor::new(
        vec![answered(302, 1), answered(302, 1), answered(200, 1)],
        vec![next(), next(), Ok(None)],
    );
    let result = block_on(run_attempt(&fake, &mut (), request(), THIRTY, 5));
    assert_eq!(result.record.hops().len(), 3);
    assert_eq!(fake.follow_calls(), 3);
    assert_eq!(result.response.unwrap().status, 200);
}

#[test]
fn redirects_zero_returns_the_3xx_without_calling_follow() {
    let fake = ScriptedExecutor::new(vec![answered(302, 1)], vec![]);
    let result = block_on(run_attempt(&fake, &mut (), request(), THIRTY, 0));
    assert_eq!(result.record.hops().len(), 1);
    assert_eq!(result.response.unwrap().status, 302);
    assert_eq!(fake.follow_calls(), 0);
}

#[test]
fn an_exhausted_limit_ends_with_too_many_redirects() {
    let fake = ScriptedExecutor::new(
        vec![answered(302, 1), answered(302, 1), answered(302, 1)],
        vec![next(), next(), next()],
    );
    let result = block_on(run_attempt(&fake, &mut (), request(), THIRTY, 2));
    assert_eq!(result.record.hops().len(), 3);
    assert_eq!(
        result.record.outcome(),
        Outcome::Failed {
            kind: FailureKind::TooManyRedirects,
            stage: FailureStage::Prepare
        }
    );
    assert!(result.response.is_none());
}

#[test]
fn a_rejected_redirect_ends_the_attempt() {
    let fake = ScriptedExecutor::new(
        vec![answered(302, 1)],
        vec![Err(failure(
            FailureKind::InvalidRedirect,
            FailureStage::Prepare,
        ))],
    );
    let result = block_on(run_attempt(&fake, &mut (), request(), THIRTY, 5));
    assert_eq!(result.record.hops().len(), 1);
    assert_eq!(
        result.record.outcome(),
        Outcome::Failed {
            kind: FailureKind::InvalidRedirect,
            stage: FailureStage::Prepare
        }
    );
    assert!(result.response.is_none());
}

#[test]
fn a_failed_hop_ends_the_attempt_without_follow() {
    let fake = ScriptedExecutor::new(
        vec![failed(FailureKind::Connect, FailureStage::Connect, 1)],
        vec![],
    );
    let result = block_on(run_attempt(&fake, &mut (), request(), THIRTY, 5));
    assert_eq!(result.record.hops().len(), 1);
    assert_eq!(
        result.record.outcome(),
        Outcome::Failed {
            kind: FailureKind::Connect,
            stage: FailureStage::Connect
        }
    );
    assert_eq!(fake.follow_calls(), 0);
    assert!(result.response.is_none());
}

#[test]
fn each_hop_gets_the_time_left() {
    let fake = ScriptedExecutor::new(
        vec![answered(302, 3), answered(302, 4), answered(200, 1)],
        vec![next(), next(), Ok(None)],
    );
    let ten = Duration::from_secs(10);
    block_on(run_attempt(&fake, &mut (), request(), ten, 5));
    let secs = |n| Duration::from_secs(n);
    assert_eq!(fake.timeouts(), [secs(10), secs(7), secs(3)]);
}

#[test]
fn no_time_left_ends_with_timeout_before_another_hop() {
    let fake = ScriptedExecutor::new(
        vec![answered(302, 3), answered(302, 4)],
        vec![next(), next()],
    );
    let seven = Duration::from_secs(7);
    let result = block_on(run_attempt(&fake, &mut (), request(), seven, 5));
    assert_eq!(result.record.hops().len(), 2);
    assert_eq!(
        result.record.outcome(),
        Outcome::Failed {
            kind: FailureKind::Timeout,
            stage: FailureStage::Prepare
        }
    );
    assert_eq!(
        fake.timeouts(),
        [Duration::from_secs(7), Duration::from_secs(4)]
    );
    assert!(result.response.is_none());
}

struct RecordingTimer {
    sleeps: Mutex<Vec<Duration>>,
}

impl RecordingTimer {
    fn new() -> Self {
        Self {
            sleeps: Mutex::new(Vec::new()),
        }
    }

    fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.lock().unwrap().clone()
    }
}

impl Timer for RecordingTimer {
    async fn sleep(&self, duration: Duration) {
        self.sleeps.lock().unwrap().push(duration);
    }
}

fn to(url: &str) -> HttpRequest {
    HttpRequest {
        url: RequestUrl::Absolute(url.to_string()),
        ..request()
    }
}

fn post() -> HttpRequest {
    HttpRequest {
        method: "POST".parse().expect("POST is a method"),
        ..request()
    }
}

fn retrying(attempts: u32, backoff_ms: u64, allow: bool) -> StepPolicy {
    StepPolicy {
        timeout: THIRTY,
        redirects: 5,
        retry: Some(RetryPolicy {
            attempts,
            backoff: Duration::from_millis(backoff_ms),
            allow_non_idempotent: allow,
        }),
    }
}

fn once() -> StepPolicy {
    StepPolicy {
        timeout: THIRTY,
        redirects: 5,
        retry: None,
    }
}

fn outcomes(result: &StepResult) -> Vec<Outcome> {
    result
        .record
        .attempts()
        .iter()
        .map(|attempt| attempt.outcome())
        .collect()
}

fn status(code: u16) -> Outcome {
    Outcome::Response {
        status: code,
        truncated: false,
    }
}

fn step(
    fake: &ScriptedExecutor,
    timer: &RecordingTimer,
    request: HttpRequest,
    policy: &StepPolicy,
) -> StepResult {
    block_on(run_step(fake, timer, &mut (), request, policy))
}

#[test]
fn without_retry_a_5xx_is_the_response() {
    let fake = ScriptedExecutor::new(vec![answered(503, 1)], vec![Ok(None)]);
    let timer = RecordingTimer::new();
    let result = step(&fake, &timer, request(), &once());
    assert_eq!(outcomes(&result), [status(503)]);
    assert_eq!(result.response.unwrap().status, 503);
    assert!(timer.sleeps().is_empty());
}

#[test]
fn a_5xx_then_a_2xx_takes_two_attempts() {
    let fake = ScriptedExecutor::new(
        vec![answered(503, 1), answered(200, 1)],
        vec![Ok(None), Ok(None)],
    );
    let timer = RecordingTimer::new();
    let result = step(&fake, &timer, request(), &retrying(2, 250, false));
    assert_eq!(outcomes(&result), [status(503), status(200)]);
    assert_eq!(result.response.unwrap().status, 200);
    assert_eq!(timer.sleeps(), [Duration::from_millis(250)]);
}

#[test]
fn transport_failures_are_retried() {
    let not_retried = [
        FailureKind::InvalidRequest,
        FailureKind::InvalidRedirect,
        FailureKind::TooManyRedirects,
    ];
    for kind in FailureKind::ALL {
        if not_retried.contains(&kind) {
            continue;
        }
        let fake = ScriptedExecutor::new(
            vec![failed(kind, FailureStage::Connect, 0), answered(200, 0)],
            vec![Ok(None)],
        );
        let timer = RecordingTimer::new();
        let result = step(&fake, &timer, request(), &retrying(2, 0, false));
        assert_eq!(result.record.attempts().len(), 2, "{kind:?}");
        assert_eq!(result.response.unwrap().status, 200, "{kind:?}");
    }
}

#[test]
fn request_and_redirect_errors_are_not_retried() {
    let timer = RecordingTimer::new();
    let one = |kind| Outcome::Failed {
        kind,
        stage: FailureStage::Prepare,
    };

    let fake = ScriptedExecutor::new(
        vec![failed(
            FailureKind::InvalidRequest,
            FailureStage::Prepare,
            0,
        )],
        vec![],
    );
    let result = step(&fake, &timer, request(), &retrying(3, 0, false));
    assert_eq!(outcomes(&result), [one(FailureKind::InvalidRequest)]);

    let fake = ScriptedExecutor::new(
        vec![answered(302, 0)],
        vec![Err(failure(
            FailureKind::InvalidRedirect,
            FailureStage::Prepare,
        ))],
    );
    let result = step(&fake, &timer, request(), &retrying(3, 0, false));
    assert_eq!(outcomes(&result), [one(FailureKind::InvalidRedirect)]);

    let fake = ScriptedExecutor::new(
        vec![answered(302, 0), answered(302, 0)],
        vec![next(), next()],
    );
    let mut policy = retrying(3, 0, false);
    policy.redirects = 1;
    let result = step(&fake, &timer, request(), &policy);
    assert_eq!(outcomes(&result), [one(FailureKind::TooManyRedirects)]);
    assert!(timer.sleeps().is_empty());
}

#[test]
fn client_errors_and_redirect_responses_are_not_retried() {
    let timer = RecordingTimer::new();

    let fake = ScriptedExecutor::new(vec![answered(404, 0)], vec![Ok(None)]);
    let result = step(&fake, &timer, request(), &retrying(3, 0, false));
    assert_eq!(result.record.attempts().len(), 1);
    assert_eq!(result.response.unwrap().status, 404);

    let fake = ScriptedExecutor::new(vec![answered(302, 0)], vec![]);
    let mut policy = retrying(3, 0, false);
    policy.redirects = 0;
    let result = step(&fake, &timer, request(), &policy);
    assert_eq!(result.record.attempts().len(), 1);
    assert_eq!(result.response.unwrap().status, 302);

    let mut truncated = answered(200, 0);
    truncated.outcome.as_mut().unwrap().truncated = true;
    let fake = ScriptedExecutor::new(vec![truncated], vec![Ok(None)]);
    let result = step(&fake, &timer, request(), &retrying(3, 0, false));
    assert_eq!(result.record.attempts().len(), 1);
    assert!(result.response.unwrap().truncated);
}

#[test]
fn every_attempt_is_made_and_the_last_5xx_is_the_response() {
    let fake = ScriptedExecutor::new(
        vec![answered(503, 0), answered(503, 0), answered(503, 0)],
        vec![Ok(None), Ok(None), Ok(None)],
    );
    let timer = RecordingTimer::new();
    let result = step(&fake, &timer, request(), &retrying(3, 100, false));
    assert_eq!(result.record.attempts().len(), 3);
    assert_eq!(timer.sleeps(), [Duration::from_millis(100); 2]);
    assert_eq!(result.response.unwrap().status, 503);
}

#[test]
fn non_idempotent_methods_retry_only_with_the_opt_in() {
    let timer = RecordingTimer::new();
    let fake = ScriptedExecutor::new(vec![answered(503, 0)], vec![Ok(None)]);
    let result = step(&fake, &timer, post(), &retrying(2, 0, false));
    assert_eq!(result.record.attempts().len(), 1);

    let fake = ScriptedExecutor::new(
        vec![answered(503, 0), answered(200, 0)],
        vec![Ok(None), Ok(None)],
    );
    let result = step(&fake, &timer, post(), &retrying(2, 0, true));
    assert_eq!(outcomes(&result), [status(503), status(200)]);
}

#[test]
fn the_original_method_decides() {
    let fake = ScriptedExecutor::new(
        vec![answered(303, 0), answered(503, 0)],
        vec![Ok(Some(to("http://127.0.0.1/next"))), Ok(None)],
    );
    let timer = RecordingTimer::new();
    let result = step(&fake, &timer, post(), &retrying(2, 0, false));
    assert_eq!(result.record.attempts().len(), 1);
    assert_eq!(result.record.attempts()[0].hops().len(), 2);
    assert_eq!(result.response.unwrap().status, 503);
}

#[test]
fn each_attempt_restarts_with_the_full_timeout() {
    let fake = ScriptedExecutor::new(
        vec![answered(302, 3), answered(503, 4), answered(200, 1)],
        vec![Ok(Some(to("http://127.0.0.1/next"))), Ok(None), Ok(None)],
    );
    let timer = RecordingTimer::new();
    let mut policy = retrying(2, 0, false);
    policy.timeout = Duration::from_secs(10);
    let result = step(&fake, &timer, request(), &policy);
    let secs = Duration::from_secs;
    assert_eq!(fake.timeouts(), [secs(10), secs(7), secs(10)]);
    let absolute = |url: &str| RequestUrl::Absolute(url.to_string());
    assert_eq!(
        fake.urls(),
        [
            absolute("http://127.0.0.1/"),
            absolute("http://127.0.0.1/next"),
            absolute("http://127.0.0.1/")
        ]
    );
    assert_eq!(result.response.unwrap().status, 200);
}

#[test]
fn fewer_than_two_attempts_make_one() {
    for attempts in [1, 0] {
        let fake = ScriptedExecutor::new(vec![answered(503, 0)], vec![Ok(None)]);
        let timer = RecordingTimer::new();
        let result = step(&fake, &timer, request(), &retrying(attempts, 0, false));
        assert_eq!(result.record.attempts().len(), 1);
        assert!(timer.sleeps().is_empty());
    }
}

#[test]
fn a_zero_backoff_still_sleeps() {
    let fake = ScriptedExecutor::new(
        vec![answered(503, 0), answered(200, 0)],
        vec![Ok(None), Ok(None)],
    );
    let timer = RecordingTimer::new();
    step(&fake, &timer, request(), &retrying(2, 0, false));
    assert_eq!(timer.sleeps(), [Duration::ZERO]);
}

#[test]
fn the_largest_step_records_210_hops() {
    let mut hops = Vec::new();
    let mut follows = Vec::new();
    for _ in 0..10 {
        for _ in 0..20 {
            hops.push(answered(302, 0));
            follows.push(next());
        }
        hops.push(answered(503, 0));
        follows.push(Ok(None));
    }
    let fake = ScriptedExecutor::new(hops, follows);
    let timer = RecordingTimer::new();
    let mut policy = retrying(10, 0, false);
    policy.redirects = 20;
    let result = step(&fake, &timer, request(), &policy);
    assert_eq!(result.record.attempts().len(), 10);
    assert!(
        result
            .record
            .attempts()
            .iter()
            .all(|a| a.hops().len() == 21)
    );
    let total: usize = result
        .record
        .attempts()
        .iter()
        .map(|a| a.hops().len())
        .sum();
    assert_eq!(total, 210);
    assert_eq!(timer.sleeps().len(), 9);
}

#[test]
fn a_step_future_is_send() {
    let fake = ScriptedExecutor::new(vec![], vec![]);
    let timer = RecordingTimer::new();
    assert_send(&run_step(&fake, &timer, &mut (), request(), &once()));
}
