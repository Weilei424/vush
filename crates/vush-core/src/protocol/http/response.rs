//! The HTTP response an executor returns, and the hop that carries it.
//!
//! Only the final response comes back; an interim `1xx` never surfaces. A
//! body cut at the executor's size limit is still a response, not a failure.
//!
//! `Debug` never shows a header value or a body byte. It shows the status, the
//! version, header names (through `{:?}`, so control characters are escaped),
//! the body's size in bytes and `truncated`.

use std::fmt;

use crate::protocol::{BodySizes, ConnectionId, Failure, HopRecord, HopTiming, Outcome};

/// The HTTP version a response arrived over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireVersion {
    /// HTTP/1.0.
    Http10,
    /// HTTP/1.1.
    Http11,
    /// HTTP/2.
    Http2,
}

/// The final response to one request. Interim `1xx` responses never surface.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The status the server sent, as the library parsed it.
    pub status: u16,
    /// The version the response arrived over.
    pub version: WireVersion,
    /// In the order received, repeats kept. Names are lowercase. Values are
    /// raw bytes, because servers send non-UTF-8 values.
    pub headers: Vec<(String, Vec<u8>)>,
    /// At most the executor's response-size limit.
    pub body: Vec<u8>,
    /// The body was longer than the limit, and `body` holds only its start.
    /// That is not a failure: the status and the headers are kept.
    pub truncated: bool,
}

/// The header names of a response, each printed through `{:?}`.
struct HeaderNames<'a>(&'a [(String, Vec<u8>)]);

impl fmt::Debug for HeaderNames<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| name))
            .finish()
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("version", &self.version)
            .field("headers", &HeaderNames(&self.headers))
            .field("body", &format_args!("<{} bytes>", self.body.len()))
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// The result of one `send` call.
///
/// Timing and the connection sit outside the `Result`, so a failed hop keeps
/// them too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpHop {
    /// How long the hop took.
    pub timing: HopTiming,
    /// The connection the hop used, if it got one.
    pub connection: Option<ConnectionId>,
    /// The body bytes sent and received.
    pub sizes: BodySizes,
    /// The response, or why there is none.
    pub outcome: Result<HttpResponse, Failure>,
}

impl HttpHop {
    /// The neutral record of this hop, without the response's headers or body.
    pub fn record(&self) -> HopRecord {
        let outcome = match &self.outcome {
            Ok(response) => Outcome::Response {
                status: response.status,
                truncated: response.truncated,
            },
            Err(failure) => Outcome::Failed {
                kind: failure.kind,
                stage: failure.stage,
            },
        };
        HopRecord {
            timing: self.timing,
            connection: self.connection,
            sizes: self.sizes,
            outcome,
        }
    }
}
