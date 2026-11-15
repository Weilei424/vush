//! The executor: one pooled hyper-util client over Vush's own connector.

use std::fmt;
use std::path::Path;
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
use crate::tls::{Trust, Verification, add_ca_file, client_config, system_roots};

/// Why an executor couldn't be built. Each text is fixed: it names a
/// setting's key, never the path it holds, the file's content or a library's
/// text.
#[derive(Debug)]
pub enum BuildError {
    /// Settings that verify certificates have no trusted root: the system
    /// has no usable root certificate, and no `tls.ca_file` adds one.
    NoTrustedRoots,
    /// The `tls.ca_file` of the settings at `at` can't be used.
    CaFile { at: Scope, problem: CaFileProblem },
}

/// Where in the test file a setting comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The file-level `http` block.
    File,
    /// The merged `http` settings of the scenario at this index.
    Scenario(usize),
}

/// Why a `tls.ca_file` can't be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaFileProblem {
    /// The file doesn't exist.
    NotFound,
    /// The file can't be read, for any other reason. A directory is one.
    Unreadable,
    /// The file is larger than 1 MiB.
    TooLarge,
    /// The file holds no PEM certificate.
    NoCertificates,
    /// A PEM section can't be decoded, or a certificate can't be used as a
    /// root.
    BadCertificate,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTrustedRoots => {
                f.write_str("no trusted root certificates could be loaded from the system")
            }
            Self::CaFile { at, problem } => {
                match at {
                    Scope::File => f.write_str("`http")?,
                    Scope::Scenario(index) => write!(f, "`scenarios[{index}].http")?,
                }
                f.write_str(".tls.ca_file` can't be used: ")?;
                f.write_str(match problem {
                    CaFileProblem::NotFound => "the file doesn't exist",
                    CaFileProblem::Unreadable => "the file can't be read",
                    CaFileProblem::TooLarge => "the file is larger than 1 MiB",
                    CaFileProblem::NoCertificates => "the file holds no PEM certificate",
                    CaFileProblem::BadCertificate => {
                        "the file holds a certificate that can't be used"
                    }
                })
            }
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
/// - `tls.ca_file` names a PEM file, resolved against the test file's
///   directory, whose certificates are trusted as well as the system's
///   roots. It is read once, while building. `tls.insecure` accepts any
///   certificate for any name, though the handshake's signatures are still
///   checked, and `tls.ca_file` is then not read.
pub struct Executor {
    client: Client<Connector, CountingBody>,
    limit: u64,
}

impl Executor {
    /// An executor that trusts the system's root certificates, plus those in
    /// `tls.ca_file`, which is resolved against `test_dir`, the test file's
    /// directory. System roots that don't parse are skipped.
    ///
    /// Building fails with [`BuildError::CaFile`] when `tls.ca_file` can't be
    /// used, and with [`BuildError::NoTrustedRoots`] when no root is left.
    /// Under `tls.insecure`, neither the system's roots nor `tls.ca_file` are
    /// read.
    pub fn new(settings: &HttpSettings, test_dir: &Path) -> Result<Self, BuildError> {
        let verification = verification(settings, test_dir, system_roots)?;
        if let Verification::Roots(roots) = &verification
            && roots.is_empty()
        {
            return Err(BuildError::NoTrustedRoots);
        }
        Ok(Self::build(settings, verification))
    }

    /// Like [`Executor::new`], with `system` standing in for the system's
    /// roots. It never fails with [`BuildError::NoTrustedRoots`], so
    /// `RootCertStore::empty()` serves plaintext servers.
    pub fn with_roots(
        settings: &HttpSettings,
        test_dir: &Path,
        system: RootCertStore,
    ) -> Result<Self, BuildError> {
        let verification = verification(settings, test_dir, || system)?;
        Ok(Self::build(settings, verification))
    }

    fn build(settings: &HttpSettings, verification: Verification) -> Self {
        let version = settings.version;
        let connector = Connector::new(client_config(verification, version), version);
        let client = Client::builder(TokioExecutor::new())
            .http2_only(version == HttpVersion::Http2)
            .build(connector);
        Self {
            client,
            limit: settings.max_response_size.0,
        }
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

/// How a lone executor checks certificates. `system` gives the system's
/// roots, and is called only when the settings verify. A `tls.ca_file`
/// problem is reported at [`Scope::File`], because a lone executor's
/// settings are file-shaped.
fn verification(
    settings: &HttpSettings,
    test_dir: &Path,
    system: impl FnOnce() -> RootCertStore,
) -> Result<Verification, BuildError> {
    let roots = match Trust::of(settings, test_dir) {
        Trust::Insecure => return Ok(Verification::Insecure),
        Trust::System => system(),
        Trust::SystemPlus(path) => {
            let mut roots = system();
            add_ca_file(&mut roots, &path).map_err(|problem| BuildError::CaFile {
                at: Scope::File,
                problem,
            })?;
            roots
        }
    };
    Ok(Verification::Roots(roots))
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
