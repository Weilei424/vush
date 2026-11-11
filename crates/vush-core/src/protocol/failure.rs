//! Why a hop failed: a kind, the stage it was in, and a fixed message.

use std::fmt;

/// What went wrong. The labels are metric dimensions and JSON values later,
/// so they are stable: renaming or removing one is a breaking change.
///
/// A `4xx` or `5xx` response is not a failure, and neither is a body cut at
/// the size limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureKind {
    /// The request can't be sent as given: a bad URL, an unsupported scheme,
    /// or a bad header name or value. Nothing was sent.
    InvalidRequest,
    /// The host name didn't resolve.
    Dns,
    /// No TCP connection: refused, unreachable, or reset while connecting.
    Connect,
    /// The TLS handshake failed: an untrusted or invalid certificate, a
    /// certificate for another name, or no common protocol.
    Tls,
    /// The hop's time ran out. The stage says where.
    Timeout,
    /// The connection closed or reset before the response was complete.
    ConnectionClosed,
    /// The response broke HTTP rules or the client's limits: a malformed
    /// status line, a head that is too large, or an HTTP/2 error.
    Protocol,
    /// A redirect that can't be followed, such as an unparsable `Location` or
    /// one with an unsupported scheme. It ends the attempt.
    InvalidRedirect,
    /// The chain needed more redirects than `http.redirects` allows, when that
    /// is 1 or more. Core's hop loop reports it, never the executor, and it
    /// ends the attempt.
    TooManyRedirects,
    /// A failure the executor can't place above. It should be rare.
    Other,
}

impl FailureKind {
    /// Every kind, in label order.
    pub const ALL: [FailureKind; 10] = [
        FailureKind::InvalidRequest,
        FailureKind::Dns,
        FailureKind::Connect,
        FailureKind::Tls,
        FailureKind::Timeout,
        FailureKind::ConnectionClosed,
        FailureKind::Protocol,
        FailureKind::InvalidRedirect,
        FailureKind::TooManyRedirects,
        FailureKind::Other,
    ];

    /// The kind's stable snake_case label.
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::InvalidRequest => "invalid_request",
            FailureKind::Dns => "dns",
            FailureKind::Connect => "connect",
            FailureKind::Tls => "tls",
            FailureKind::Timeout => "timeout",
            FailureKind::ConnectionClosed => "connection_closed",
            FailureKind::Protocol => "protocol",
            FailureKind::InvalidRedirect => "invalid_redirect",
            FailureKind::TooManyRedirects => "too_many_redirects",
            FailureKind::Other => "other",
        }
    }
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Where in a hop a failure happened, as far as the executor knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureStage {
    /// Before anything was sent: checking and encoding the request, or
    /// working out a redirect's next request.
    Prepare,
    /// Resolving the name, connecting, and the TLS handshake.
    Connect,
    /// Sending the request and waiting for the response head.
    Headers,
    /// Reading the response body.
    Body,
}

impl FailureStage {
    /// Every stage, in label order.
    pub const ALL: [FailureStage; 4] = [
        FailureStage::Prepare,
        FailureStage::Connect,
        FailureStage::Headers,
        FailureStage::Body,
    ];

    /// The stage's stable snake_case label.
    pub fn label(self) -> &'static str {
        match self {
            FailureStage::Prepare => "prepare",
            FailureStage::Connect => "connect",
            FailureStage::Headers => "headers",
            FailureStage::Body => "body",
        }
    }
}

impl fmt::Display for FailureStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A failed hop or attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    /// What went wrong.
    pub kind: FailureKind,
    /// Where it went wrong.
    pub stage: FailureStage,
    /// A short fixed text such as "connection refused". It is chosen from the
    /// executor's own texts and never built from the request, the response or
    /// a library's error.
    pub message: &'static str,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} in {}: {}", self.kind, self.stage, self.message)
    }
}

impl std::error::Error for Failure {}
