//! The attempt loop, driven by a scripted executor.

use std::collections::VecDeque;
use std::pin::pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use vush_core::protocol::http::{
    HttpExecutor, HttpHop, HttpRequest, HttpResponse, RequestUrl, WireVersion,
};
use vush_core::protocol::{BodySizes, Failure, FailureKind, FailureStage, HopTiming, Outcome};
use vush_core::runner::run_attempt;

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
    follow_calls: AtomicUsize,
}

impl ScriptedExecutor {
    fn new(hops: Vec<HttpHop>, follows: Vec<Result<Option<HttpRequest>, Failure>>) -> Self {
        Self {
            hops: Mutex::new(hops.into()),
            follows: Mutex::new(follows.into()),
            timeouts: Mutex::new(Vec::new()),
            follow_calls: AtomicUsize::new(0),
        }
    }

    fn timeouts(&self) -> Vec<Duration> {
        self.timeouts.lock().unwrap().clone()
    }

    fn follow_calls(&self) -> usize {
        self.follow_calls.load(Ordering::SeqCst)
    }
}

impl HttpExecutor for ScriptedExecutor {
    type Session = ();

    fn new_session(&self) {}

    async fn send(&self, _session: &mut (), _request: &HttpRequest, timeout: Duration) -> HttpHop {
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
