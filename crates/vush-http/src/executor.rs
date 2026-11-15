//! The executor: one pooled hyper-util client over Vush's own connector.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use http::{Extensions, Request, Version};
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{CaptureConnection, Connected, capture_connection};
use hyper_util::rt::TokioExecutor;
use rustls::RootCertStore;
use vush_core::plan::{HttpSettings, HttpVersion};
use vush_core::protocol::http::{HttpExecutor, HttpHop, HttpRequest, HttpResponse, WireVersion};
use vush_core::protocol::{BodySizes, Failure, FailureKind, FailureStage, HopTiming};

use crate::body::CountingBody;
use crate::classify::{classify, failure, timeout};
use crate::connector::{ConnInfo, Connector};
use crate::prepare::{Prepared, prepare};
use crate::stage::StageMarker;
use crate::tls::client_config;

/// Why an executor couldn't be built. Each text is fixed.
#[derive(Debug)]
pub enum BuildError {
    /// [`Executor::new`] found no system root certificate it could use.
    NoTrustedRoots,
    /// The settings use a key that isn't supported yet. It holds the key,
    /// `tls.ca_file` or `tls.insecure`.
    Unsupported(&'static str),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTrustedRoots => {
                f.write_str("no trusted root certificates could be loaded from the system")
            }
            Self::Unsupported(key) => write!(f, "`{key}` isn't supported yet"),
        }
    }
}

impl std::error::Error for BuildError {}

/// One user's protocol state. It holds nothing yet: P2.3 fills it with the
/// user's own pool under `pool: vu`, and P2.4 with the cookie jar.
#[derive(Clone, Default)]
pub struct Session {}

/// Sends HTTP/1.1 and HTTP/2 requests, in plaintext or over TLS, through one
/// pooled client.
///
/// `send` must run inside a Tokio runtime, of either kind, because the client
/// spawns its connection tasks on it. Building needs no runtime.
///
/// How the [`HttpSettings`] are read:
/// - `version` sets the version mode: `auto` negotiates over TLS, `http1`
///   speaks only HTTP/1.1, and `http2` speaks only HTTP/2, with prior
///   knowledge in plaintext.
/// - `max_response_size` is the body limit. A longer body is cut at it and
///   marked `truncated`.
/// - `pool` is treated as `shared` until P2.3: an executor has one pool.
/// - `cookies` has no effect until P2.4, because the session keeps nothing.
/// - `timeout` and `redirects` are core's. Core passes the time left to each
///   `send`, and follows redirects itself.
/// - `tls.ca_file` and `tls.insecure` make building fail with
///   [`BuildError::Unsupported`] until P2.3.
pub struct Executor {
    client: Client<Connector, CountingBody>,
    limit: u64,
}

impl Executor {
    /// An executor that trusts the system's root certificates. Those that
    /// don't parse are skipped, and if none is left, building fails with
    /// [`BuildError::NoTrustedRoots`].
    pub fn new(settings: &HttpSettings) -> Result<Self, BuildError> {
        supported(settings)?;
        let mut roots = RootCertStore::empty();
        let (added, _skipped) =
            roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        if added == 0 {
            return Err(BuildError::NoTrustedRoots);
        }
        Self::with_roots(settings, roots)
    }

    /// An executor that trusts exactly `roots`.
    pub fn with_roots(settings: &HttpSettings, roots: RootCertStore) -> Result<Self, BuildError> {
        supported(settings)?;
        let version = settings.version;
        let connector = Connector::new(client_config(roots, version), version);
        let client = Client::builder(TokioExecutor::new())
            .http2_only(version == HttpVersion::Http2)
            .build(connector);
        Ok(Self {
            client,
            limit: settings.max_response_size.0,
        })
    }

    /// Runs the rest of the hop until the hop's deadline, and reports a
    /// timeout in the stage the marker shows. With no deadline, the hop
    /// never runs out of time.
    async fn exchange(
        &self,
        request: Request<CountingBody>,
        capture: &mut CaptureConnection,
        deadline: Option<Instant>,
        progress: &mut Progress,
    ) -> Result<HttpResponse, Failure> {
        // `prepare` is done, so the hop is waiting for a connection: its own
        // connect, a pooled one, or, under `http2`, another hop's connect,
        // which hyper-util shares without running the connector for this hop.
        let marker = StageMarker::new();
        marker.set(FailureStage::Connect);
        let hop = self.drive(request, capture, &marker, progress);
        let Some(deadline) = deadline else {
            return hop.await;
        };
        match tokio::time::timeout_at(deadline.into(), hop).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => Err(timeout(marker.get())),
        }
    }

    /// Sends the request, waits for its head, and reads its body up to the
    /// limit.
    async fn drive(
        &self,
        request: Request<CountingBody>,
        capture: &mut CaptureConnection,
        marker: &StageMarker,
        progress: &mut Progress,
    ) -> Result<HttpResponse, Failure> {
        let mut response = self.client.request(request);
        // The hop is in `connect` until hyper-util assigns it a connection,
        // new or pooled. Only that tells this hop apart: a connect that loses
        // to a pooled connection finishes on another task.
        let head = tokio::select! {
            biased;
            head = &mut response => head,
            assigned = connection_assigned(capture) => {
                if assigned {
                    marker.set(FailureStage::Headers);
                }
                response.await
            }
        };
        // A head that failed in the same poll that assigned its connection
        // got there before the marker moved.
        if capture.connection_metadata().is_some() {
            marker.set(FailureStage::Headers);
        }
        // A failure before the head is in the marker's stage, unless its
        // cause places it.
        let head = match head {
            Ok(head) => head,
            Err(error) => {
                progress.error_connection = error.connect_info().and_then(conn_info);
                return Err(classify(&error, marker.get()));
            }
        };
        progress.to_headers = Some(progress.start.elapsed());
        marker.set(FailureStage::Body);

        let (parts, mut body) = head.into_parts();
        let version = wire_version(parts.version)
            .ok_or_else(|| failure(FailureKind::Protocol, FailureStage::Headers))?;
        let mut truncated = false;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|error| classify(&error, FailureStage::Body))?;
            // Trailers are dropped.
            let Ok(data) = frame.into_data() else {
                continue;
            };
            let kept = progress.body.len() as u64;
            let room = usize::try_from(self.limit.saturating_sub(kept)).unwrap_or(usize::MAX);
            if data.len() > room {
                // The rest is dropped unread, which closes an HTTP/1.1
                // connection.
                progress.body.extend_from_slice(&data[..room]);
                truncated = true;
                break;
            }
            progress.body.extend_from_slice(&data);
        }
        let headers = parts
            .headers
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
            .collect();
        Ok(HttpResponse {
            status: parts.status.as_u16(),
            version,
            headers,
            body: std::mem::take(&mut progress.body),
            truncated,
        })
    }
}

impl HttpExecutor for Executor {
    type Session = Session;

    fn new_session(&self) -> Session {
        Session::default()
    }

    async fn send(
        &self,
        _session: &mut Session,
        request: &HttpRequest,
        timeout_after: Duration,
    ) -> HttpHop {
        let mut progress = Progress::new();
        // A timeout too long to add to the start has no deadline.
        let deadline = progress.start.checked_add(timeout_after);
        let sent = Arc::new(AtomicU64::new(0));
        let mut capture = None;
        let outcome = if timeout_after.is_zero() {
            Err(timeout(FailureStage::Prepare))
        } else {
            match prepare(request) {
                Err(failure) => Err(failure),
                // `prepare` can't be cut short, so the deadline is checked
                // once it is done.
                Ok(_) if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                    Err(timeout(FailureStage::Prepare))
                }
                Ok(prepared) => {
                    let mut request = wire_request(prepared, sent.clone());
                    let capture = capture.insert(capture_connection(&mut request));
                    self.exchange(request, capture, deadline, &mut progress)
                        .await
                }
            }
        };

        let info = capture
            .as_ref()
            .and_then(|capture| capture.connection_metadata().as_ref().and_then(conn_info))
            .or(progress.error_connection);
        let connection = info.as_ref().map(|info| info.id);
        let setup = info.and_then(|info| info.take_setup());
        let received = match &outcome {
            Ok(response) => response.body.len(),
            Err(_) => progress.body.len(),
        };
        HttpHop {
            timing: HopTiming {
                to_headers: progress.to_headers,
                total: progress.start.elapsed(),
                setup,
            },
            connection,
            sizes: BodySizes {
                sent: sent.load(Ordering::Relaxed),
                received: received as u64,
            },
            outcome,
        }
    }

    fn follow(
        &self,
        request: &HttpRequest,
        response: &HttpResponse,
    ) -> Result<Option<HttpRequest>, Failure> {
        crate::follow::follow(request, response)
    }
}

/// What a hop has learned so far. It lives outside the hop's timeout, so a
/// hop that runs out of time keeps what it learned first.
struct Progress {
    start: Instant,
    to_headers: Option<Duration>,
    /// The body bytes kept so far. On a failure, they count as received.
    body: Vec<u8>,
    /// The connection a hyper-util error names, if any.
    error_connection: Option<Arc<ConnInfo>>,
}

impl Progress {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            to_headers: None,
            body: Vec::new(),
            error_connection: None,
        }
    }
}

fn supported(settings: &HttpSettings) -> Result<(), BuildError> {
    if settings.tls.ca_file.is_some() {
        return Err(BuildError::Unsupported("tls.ca_file"));
    }
    if settings.tls.insecure {
        return Err(BuildError::Unsupported("tls.insecure"));
    }
    Ok(())
}

fn wire_request(prepared: Prepared, sent: Arc<AtomicU64>) -> Request<CountingBody> {
    let mut request = Request::new(CountingBody::new(prepared.body, sent));
    *request.method_mut() = prepared.method;
    *request.uri_mut() = prepared.uri;
    *request.headers_mut() = prepared.headers;
    request
}

/// Resolves once hyper-util assigns a connection to the request, new or
/// pooled. It gives `false` if the request was dropped without one.
async fn connection_assigned(capture: &mut CaptureConnection) -> bool {
    capture.wait_for_connection_metadata().await.is_some()
}

/// The info the connector attached to a connection.
fn conn_info(connected: &Connected) -> Option<Arc<ConnInfo>> {
    let mut extensions = Extensions::new();
    connected.get_extras(&mut extensions);
    extensions.remove::<Arc<ConnInfo>>()
}

/// hyper's client only reads HTTP/1.0, HTTP/1.1 and HTTP/2 responses.
fn wire_version(version: Version) -> Option<WireVersion> {
    match version {
        Version::HTTP_10 => Some(WireVersion::Http10),
        Version::HTTP_11 => Some(WireVersion::Http11),
        Version::HTTP_2 => Some(WireVersion::Http2),
        _ => None,
    }
}
