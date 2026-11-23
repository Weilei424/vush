//! Turning an error into a failure kind by its type, never by its text.

use std::error::Error;
use std::io;

use hyper_util::client::legacy::Error as ClientError;
use vush_core::protocol::{Failure, FailureKind, FailureStage};

use crate::connector::ConnectError;
use crate::text;

/// The fixed message of each kind `send` reports beyond `prepare` and the
/// connector's own. The connector reports its own `connect` failures, so
/// this one is for a hop that waited for another hop's connect.
fn message(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Timeout => text::HOP_TIMED_OUT,
        FailureKind::ConnectionClosed => text::CONNECTION_CLOSED,
        FailureKind::Protocol => text::PROTOCOL_BROKEN,
        FailureKind::Connect => text::SHARED_CONNECT_FAILED,
        _ => text::UNCLASSIFIED,
    }
}

/// A failure of `kind` in `stage`, with the kind's fixed message.
pub(crate) fn failure(kind: FailureKind, stage: FailureStage) -> Failure {
    Failure {
        kind,
        stage,
        message: message(kind),
    }
}

/// The hop ran out of time in `stage`.
pub(crate) fn timeout(stage: FailureStage) -> Failure {
    failure(FailureKind::Timeout, stage)
}

/// `error` and its causes, outermost first. An `io::Error` that wraps another
/// error leads to it, because `io::Error::source` skips the wrapped error.
fn chain<'a>(error: &'a (dyn Error + 'static)) -> impl Iterator<Item = &'a (dyn Error + 'static)> {
    std::iter::successors(Some(error), |&error| {
        match error.downcast_ref::<io::Error>() {
            Some(io) => io.get_ref().map(|inner| inner as &(dyn Error + 'static)),
            None => error.source(),
        }
    })
}

/// The failure for an error the connector returned. It keeps its own kind
/// and message, in `prepare` for `invalid_request` (nothing was sent) and in
/// `connect` otherwise.
pub(crate) fn connector_failure(error: ConnectError) -> Failure {
    let stage = match error.kind {
        FailureKind::InvalidRequest => FailureStage::Prepare,
        _ => FailureStage::Connect,
    };
    Failure {
        kind: error.kind,
        stage,
        message: error.message,
    }
}

/// Whether `failure` is the one a hop gets when it waited for another hop's
/// connect and learned nothing more.
pub(crate) fn is_shared_connect(failure: &Failure) -> bool {
    failure.kind == FailureKind::Connect && failure.message == text::SHARED_CONNECT_FAILED
}

/// Classifies an error from hyper-util's client or a response body.
///
/// The whole chain is searched for each cause in turn: a connector error,
/// then a hyper-util connect error with no connector error beneath it, then
/// an HTTP/2 error, then a hyper error, then malformed data, then any other
/// I/O error.
///
/// A connector error keeps its own kind, in `prepare` for `invalid_request`
/// (nothing was sent) and in `connect` otherwise. A connect error without
/// one is `connect` in `connect`. Every other failure is in `stage`.
pub(crate) fn classify(error: &(dyn Error + 'static), stage: FailureStage) -> Failure {
    if let Some(error) = chain(error).find_map(|e| e.downcast_ref::<ConnectError>()) {
        return connector_failure(*error);
    }
    // Under `http2`, hyper-util lets one hop per origin connect at a time,
    // and the others wait for it. When that connect fails or goes away, each
    // waiter gets a connect error that no connector error caused. Such a
    // waiting hop's failure comes back as `connect` here, and the executor
    // replaces it with its pool's record when one fits. The hop isn't sent
    // again: the waiters would queue behind each new connect, and a cascade
    // would settle only one of them per round.
    if chain(error)
        .filter_map(|e| e.downcast_ref::<ClientError>())
        .any(ClientError::is_connect)
    {
        return failure(FailureKind::Connect, FailureStage::Connect);
    }
    if let Some(error) = chain(error).find_map(|e| e.downcast_ref::<h2::Error>()) {
        // A reset or GOAWAY the peer sent closes the stream or the
        // connection. Anything h2 itself caught is a protocol violation.
        let closed =
            error.is_io() || (error.is_remote() && (error.is_reset() || error.is_go_away()));
        let kind = if closed {
            FailureKind::ConnectionClosed
        } else {
            FailureKind::Protocol
        };
        return failure(kind, stage);
    }
    for error in chain(error).filter_map(|e| e.downcast_ref::<hyper::Error>()) {
        if error.is_parse_too_large() {
            return Failure {
                kind: FailureKind::Protocol,
                stage,
                message: text::HEAD_TOO_LARGE,
            };
        }
        if error.is_parse() {
            return failure(FailureKind::Protocol, stage);
        }
        if error.is_incomplete_message() || error.is_closed() || error.is_canceled() {
            return failure(FailureKind::ConnectionClosed, stage);
        }
    }
    if chain(error).any(malformed) {
        return failure(FailureKind::Protocol, stage);
    }
    if chain(error).any(|e| e.is::<io::Error>()) {
        return failure(FailureKind::ConnectionClosed, stage);
    }
    failure(FailureKind::Other, stage)
}

/// Whether `error` is an I/O error for bytes that break the rules, such as
/// the size line of a chunked body that isn't hex. tokio-rustls reports a
/// broken TLS record the same way, so an error that wraps a TLS or a
/// connector error isn't malformed data: TLS broke, not HTTP.
fn malformed(error: &(dyn Error + 'static)) -> bool {
    let Some(io) = error.downcast_ref::<io::Error>() else {
        return false;
    };
    matches!(
        io.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput
    ) && !chain(error)
        .skip(1)
        .any(|e| e.is::<rustls::Error>() || e.is::<ConnectError>())
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use http_body_util::Empty;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    use super::*;
    use crate::connector::Conn;

    /// An error of no known type, with an optional cause.
    #[derive(Debug)]
    struct Wrapper(Option<Box<dyn Error + Send + Sync>>);

    impl fmt::Display for Wrapper {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("wrapper")
        }
    }

    impl Error for Wrapper {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.as_deref().map(|e| e as &(dyn Error + 'static))
        }
    }

    fn wrapped(inner: impl Error + Send + Sync + 'static) -> Wrapper {
        Wrapper(Some(Box::new(inner)))
    }

    fn kind_and_stage(
        error: &(dyn Error + 'static),
        stage: FailureStage,
    ) -> (FailureKind, FailureStage) {
        let failure = classify(error, stage);
        (failure.kind, failure.stage)
    }

    /// A connector whose connects never finish.
    #[derive(Clone)]
    struct Stalled;

    impl tower_service::Service<http::Uri> for Stalled {
        type Response = Conn;
        type Error = ConnectError;
        type Future = Pin<Box<dyn Future<Output = Result<Conn, ConnectError>> + Send>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), ConnectError>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: http::Uri) -> Self::Future {
            Box::pin(std::future::pending())
        }
    }

    /// Polls `future` once, so it runs as far as it can without waiting.
    async fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
        std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut *future).poll(cx))).await
    }

    /// Under `http2`, hyper-util lets one request per origin connect at a
    /// time, and the others wait for that connect. When it goes away, each
    /// waiter gets a connect error with no `ConnectError` beneath it. Only
    /// hyper-util can build that error, so its own client makes one here,
    /// without a socket.
    #[tokio::test]
    async fn a_cancelled_wait_for_a_shared_connect_is_connect() {
        let client: Client<Stalled, Empty<Bytes>> = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build(Stalled);
        let request = || {
            http::Request::get("http://127.0.0.1:1/")
                .body(Empty::new())
                .unwrap()
        };
        let mut owner = client.request(request());
        let mut waiter = client.request(request());
        // The first request starts the shared connect, and the second waits.
        assert!(poll_once(&mut owner).await.is_pending());
        assert!(poll_once(&mut waiter).await.is_pending());
        drop(owner);
        let error = waiter.await.expect_err("the shared connect went away");
        assert!(error.is_connect());
        assert_eq!(
            classify(&error, FailureStage::Headers),
            Failure {
                kind: FailureKind::Connect,
                stage: FailureStage::Connect,
                message: "the shared connection attempt failed",
            }
        );
    }

    #[test]
    fn connect_errors_keep_their_kind_through_wrapping() {
        for (kind, stage) in [
            (FailureKind::InvalidRequest, FailureStage::Prepare),
            (FailureKind::Dns, FailureStage::Connect),
            (FailureKind::Connect, FailureStage::Connect),
            (FailureKind::Tls, FailureStage::Connect),
        ] {
            let connect = ConnectError {
                kind,
                message: "a fixed text",
            };
            let in_io = wrapped(io::Error::other(connect));
            let failure = classify(&in_io, FailureStage::Headers);
            assert_eq!(
                failure,
                Failure {
                    kind,
                    stage,
                    message: "a fixed text"
                }
            );
        }
    }

    /// h2's public API can only make an error from a reason, which the peer
    /// never sent. A reset from the peer is tested against a real server.
    #[test]
    fn http2_errors_the_peer_didnt_send_are_protocol() {
        let local = h2::Error::from(h2::Reason::PROTOCOL_ERROR);
        assert_eq!(
            kind_and_stage(&wrapped(local), FailureStage::Headers),
            (FailureKind::Protocol, FailureStage::Headers)
        );
        // h2 comes before I/O, whatever wraps what.
        let in_io = io::Error::other(h2::Error::from(h2::Reason::FRAME_SIZE_ERROR));
        assert_eq!(
            kind_and_stage(&in_io, FailureStage::Body),
            (FailureKind::Protocol, FailureStage::Body)
        );
    }

    #[test]
    fn io_errors_are_connection_closed() {
        let reset = io::Error::from(io::ErrorKind::ConnectionReset);
        assert_eq!(
            kind_and_stage(&wrapped(reset), FailureStage::Headers),
            (FailureKind::ConnectionClosed, FailureStage::Headers)
        );
        let other = io::Error::other(Wrapper(None));
        assert_eq!(
            kind_and_stage(&other, FailureStage::Body),
            (FailureKind::ConnectionClosed, FailureStage::Body)
        );
    }

    #[test]
    fn malformed_data_is_protocol_unless_tls_reported_it() {
        for kind in [io::ErrorKind::InvalidData, io::ErrorKind::InvalidInput] {
            let malformed = wrapped(io::Error::new(kind, "invalid chunk size line"));
            assert_eq!(
                kind_and_stage(&malformed, FailureStage::Body),
                (FailureKind::Protocol, FailureStage::Body),
                "{kind:?}"
            );
        }
        // tokio-rustls reports a broken TLS record as invalid data.
        let tls = io::Error::new(io::ErrorKind::InvalidData, rustls::Error::DecryptError);
        assert_eq!(
            kind_and_stage(&wrapped(tls), FailureStage::Body),
            (FailureKind::ConnectionClosed, FailureStage::Body)
        );
    }

    #[test]
    fn unknown_errors_are_other() {
        assert_eq!(
            kind_and_stage(&wrapped(Wrapper(None)), FailureStage::Headers),
            (FailureKind::Other, FailureStage::Headers)
        );
    }

    #[test]
    fn each_kind_has_one_fixed_message() {
        let cases = [
            (FailureKind::Timeout, "the hop ran out of time"),
            (
                FailureKind::ConnectionClosed,
                "the connection closed before the response was complete",
            ),
            (FailureKind::Protocol, "the response broke HTTP rules"),
            (FailureKind::Connect, "the shared connection attempt failed"),
            (
                FailureKind::Other,
                "the request failed for an unclassified reason",
            ),
        ];
        for (kind, text) in cases {
            for stage in FailureStage::ALL {
                assert_eq!(failure(kind, stage).message, text);
            }
        }
        assert_eq!(
            timeout(FailureStage::Body),
            failure(FailureKind::Timeout, FailureStage::Body)
        );
    }
}
