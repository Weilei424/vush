//! One attempt at a request: the hop loop.

use std::time::Duration;

use crate::protocol::http::{HttpExecutor, HttpRequest, HttpResponse};
use crate::protocol::{AttemptRecord, FailureKind, FailureStage};

/// What [`run_attempt`] leaves behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptResult {
    /// Every hop of the attempt, and its ending if it has one.
    pub record: AttemptRecord,
    /// The final response, present only when `record.outcome()` is a response.
    pub response: Option<HttpResponse>,
}

/// Runs one attempt: sends `request`, and follows redirects while the
/// executor's `follow` asks for them.
///
/// - A limit of N `redirects` allows at most N + 1 hops. With 0, nothing is
///   followed and a `3xx` is the response.
/// - The hops share `timeout`. Each `send` gets the time left, which is
///   `timeout` minus the `timing.total` of the hops so far, and an attempt
///   with no time left ends with `timeout` in `prepare` before another hop.
///   Core has no clock: `follow` does no I/O, so the time between hops is
///   negligible.
/// - Intermediate responses are dropped once `follow` has read them. Only
///   their hop records stay.
///
/// The future is `Send` whenever the executor's futures are.
pub async fn run_attempt<E: HttpExecutor>(
    executor: &E,
    session: &mut E::Session,
    request: HttpRequest,
    timeout: Duration,
    redirects: u32,
) -> AttemptResult {
    let mut request = request;
    let mut followed: u32 = 0;
    let mut spent = Duration::ZERO;
    let mut record: Option<AttemptRecord> = None;
    loop {
        let left = timeout.saturating_sub(spent);
        let hop = executor.send(session, &request, left).await;
        spent = spent.saturating_add(hop.timing.total);
        let hop_record = hop.record();
        match record.as_mut() {
            Some(record) => record.push(hop_record),
            None => record = Some(AttemptRecord::new(hop_record)),
        }
        let mut record_now = record.take().expect("the record was just set");
        let response = match hop.outcome {
            Ok(response) => response,
            Err(_) => {
                return AttemptResult {
                    record: record_now,
                    response: None,
                };
            }
        };
        if redirects == 0 {
            return AttemptResult {
                record: record_now,
                response: Some(response),
            };
        }
        match executor.follow(&request, &response) {
            Ok(None) => {
                return AttemptResult {
                    record: record_now,
                    response: Some(response),
                };
            }
            Err(failure) => {
                record_now.end_with(failure.kind, failure.stage);
                return AttemptResult {
                    record: record_now,
                    response: None,
                };
            }
            Ok(Some(next)) => {
                if followed == redirects {
                    record_now.end_with(FailureKind::TooManyRedirects, FailureStage::Prepare);
                    return AttemptResult {
                        record: record_now,
                        response: None,
                    };
                }
                if spent >= timeout {
                    record_now.end_with(FailureKind::Timeout, FailureStage::Prepare);
                    return AttemptResult {
                        record: record_now,
                        response: None,
                    };
                }
                followed += 1;
                request = next;
                record = Some(record_now);
            }
        }
    }
}
