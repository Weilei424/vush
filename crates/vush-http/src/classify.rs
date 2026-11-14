//! Turning an error into a failure kind by its type, never by its text.

use std::error::Error;
use std::io;

use vush_core::protocol::{Failure, FailureKind, FailureStage};

use crate::connector::ConnectError;

/// The fixed message of each kind `send` reports beyond `prepare` and the
/// connector's own.
fn message(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Timeout => "the hop ran out of time",
        FailureKind::ConnectionClosed => "the connection closed before the response was complete",
        FailureKind::Protocol => "the response broke HTTP rules",
        _ => "the request failed for an unclassified reason",
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

/// Classifies an error from hyper-util's client or a response body.
///
/// The whole chain is searched for each cause in turn: a connector error,
/// then an HTTP/2 error, then a hyper error, then an I/O error. A connector
/// error keeps its own kind, in `prepare` for `invalid_request` (nothing was
/// sent) and in `connect` otherwise. Every other failure is in `stage`.
pub(crate) fn classify(error: &(dyn Error + 'static), stage: FailureStage) -> Failure {
    if let Some(error) = chain(error).find_map(|e| e.downcast_ref::<ConnectError>()) {
        let stage = match error.kind {
            FailureKind::InvalidRequest => FailureStage::Prepare,
            _ => FailureStage::Connect,
        };
        return Failure {
            kind: error.kind,
            stage,
            message: error.message,
        };
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
        if error.is_parse() {
            return failure(FailureKind::Protocol, stage);
        }
        if error.is_incomplete_message() || error.is_closed() || error.is_canceled() {
            return failure(FailureKind::ConnectionClosed, stage);
        }
    }
    if chain(error).any(|e| e.is::<io::Error>()) {
        return failure(FailureKind::ConnectionClosed, stage);
    }
    failure(FailureKind::Other, stage)
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::*;

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
