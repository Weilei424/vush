//! One logical request step: up to N physical attempts.

use std::time::Duration;

use crate::plan::RetryPolicy;
use crate::protocol::http::{HttpExecutor, HttpRequest, HttpResponse};
use crate::protocol::{FailureKind, Outcome, StepRecord};

use super::attempt::run_attempt;

/// Waits for a delay. Core has no clock, so the caller supplies the timer.
pub trait Timer: Send + Sync {
    /// Completes after `duration`. A zero `duration` may complete at once.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

/// How [`run_step`] runs a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepPolicy {
    /// The time limit of each attempt.
    pub timeout: Duration,
    /// The redirect limit of each attempt.
    pub redirects: u32,
    /// Retries, off when `None`.
    pub retry: Option<RetryPolicy>,
}

/// What [`run_step`] leaves behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    /// Every attempt, in order.
    pub record: StepRecord,
    /// The last attempt's response, present only when it ended in one.
    pub response: Option<HttpResponse>,
}

/// Runs one request step as one or more attempts, each one a
/// [`run_attempt`] redirect loop.
///
/// - The step makes `retry.attempts` attempts when `retry` is set, `attempts`
///   is at least 2, and the original request's method is idempotent or
///   `allow_non_idempotent` is set. Otherwise it makes one. A redirect that
///   changes the method doesn't change this.
/// - After an attempt that isn't the last, the step retries when the attempt
///   ended with a `5xx` response, or failed with any kind except
///   `invalid_request`, `invalid_redirect` and `too_many_redirects`.
/// - Each attempt starts again from the original request with the full
///   `timeout`, and uses the same session. Between attempts the step waits
///   `backoff` through `timer`, even when it is zero, and never after the
///   last attempt.
/// - The step takes at most `attempts × timeout + (attempts − 1) × backoff`.
///   Core has no clock, so the caller supplies the `timer`.
///
/// The future is `Send` whenever the executor's and the timer's futures are.
pub async fn run_step<E: HttpExecutor, T: Timer>(
    executor: &E,
    timer: &T,
    session: &mut E::Session,
    request: HttpRequest,
    policy: &StepPolicy,
) -> StepResult {
    let (attempts, backoff) = match &policy.retry {
        Some(retry)
            if retry.attempts >= 2
                && (request.method.is_idempotent() || retry.allow_non_idempotent) =>
        {
            (retry.attempts, retry.backoff)
        }
        _ => (1, Duration::ZERO),
    };

    let mut made = 1;
    let first = run_attempt(
        executor,
        session,
        request.clone(),
        policy.timeout,
        policy.redirects,
    )
    .await;
    let mut record = StepRecord::new(first.record);
    let mut response = first.response;
    while made < attempts && is_retryable(record.attempts().last().map(|a| a.outcome())) {
        timer.sleep(backoff).await;
        let next = run_attempt(
            executor,
            session,
            request.clone(),
            policy.timeout,
            policy.redirects,
        )
        .await;
        record.push(next.record);
        response = next.response;
        made += 1;
    }
    StepResult { record, response }
}

fn is_retryable(outcome: Option<Outcome>) -> bool {
    match outcome {
        Some(Outcome::Response { status, .. }) => (500..=599).contains(&status),
        Some(Outcome::Failed { kind, .. }) => !matches!(
            kind,
            FailureKind::InvalidRequest
                | FailureKind::InvalidRedirect
                | FailureKind::TooManyRedirects
        ),
        None => false,
    }
}
