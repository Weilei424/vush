//! The executor: hyper-util clients over Vush's own connector. It sends
//! through its pool, or under `pool: vu` through the session's.

use std::fmt;
use std::future::Future;
use std::num::NonZeroU32;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;
use std::time::{Duration, Instant, SystemTime};

use http::header::{COOKIE, SET_COOKIE};
use http::{Extensions, HeaderValue, Request, Version};
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{CaptureConnection, Connected, capture_connection};
use hyper_util::rt::{TokioExecutor, TokioTimer};
use rustls::RootCertStore;
use vush_core::plan::{HttpSettings, HttpVersion, ResourceLimits};
use vush_core::protocol::http::{HttpExecutor, HttpHop, HttpRequest, HttpResponse, WireVersion};
use vush_core::protocol::{
    BodySizes, ConnectionId, ConnectionSetup, Failure, FailureKind, FailureStage, HopTiming,
};

use crate::body::CountingBody;
use crate::capture::Capture;
use crate::classify::{classify, connector_failure, failure, is_shared_connect, timeout};
use crate::connector::{ConnInfo, ConnectLog, Connector};
use crate::cookie::{Jar, Target};
use crate::decode;
use crate::executors::{SystemRoots, build};
use crate::pool::ScopeView;
use crate::prepare::{Prepared, add_accept_encoding, add_cookies, prepare};
use crate::stage::{StageMarker, Unwritten};
use crate::text;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// The file can't be read, for any other reason. A path that isn't a
    /// regular file, such as a directory or a named pipe, is one.
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

/// How long a pool keeps an idle connection before it closes it.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// The ID the next executor gets. IDs are never reused in a process.
static NEXT_EXECUTOR: AtomicU64 = AtomicU64::new(0);

/// One user's protocol state. It holds the ID of the executor that made it,
/// which `send` checks; under `pool: vu` the user's own pool, with its own
/// TLS session cache, so users never share connections or resume each
/// other's sessions; and the user's cookie jar, unless the executor keeps
/// no cookies. A clone shares the pool and copies the jar, so the copy's
/// cookies change on their own.
#[derive(Clone)]
pub struct Session {
    owner: u64,
    pool: Option<Pool>,
    jar: Option<Jar>,
}

/// The read buffer limit hyper gets for HTTP/1.1: 417,792 bytes, which is
/// 408 KiB, hyper's own default set here so it can't move. hyper parses
/// what it has before it checks the size, and fails a head as too large only
/// when the head is still incomplete with at least this much buffered. So a
/// head of up to this size always passes, within hyper's default of 100
/// headers. One read fills all of the buffer's spare capacity, which can be
/// more than the limit leaves, so a longer head passes when one read brings
/// it in. Memory stays bounded by hyper's read buffer. hyper's client has no
/// exact head-size setting: `max_header_size` exists only on its server
/// builder.
pub(crate) const HEAD_BUFFER_BYTES: usize = 417_792;

/// The most bytes an HTTP/2 response header list may hold: 16 KiB. The
/// client advertises it, so a server that honors it sends no more.
pub(crate) const H2_HEADER_LIST_BYTES: u32 = 16_384;

/// A pool of connections and the log of its connects. A clone shares both.
#[derive(Clone)]
pub(crate) struct Pool {
    client: Client<Connector, CountingBody>,
    connects: ConnectLog,
}

/// Builds a pool. Every pool, pooled or per user, is built here, so each
/// closes its idle connections after [`IDLE_TIMEOUT`]. It takes the log of
/// its connects from `connector`, whose clones all share it.
pub(crate) fn new_pool(connector: Connector, version: HttpVersion) -> Pool {
    let connects = connector.connects();
    let client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(IDLE_TIMEOUT)
        .pool_timer(TokioTimer::new())
        .http2_only(version == HttpVersion::Http2)
        .http1_max_buf_size(HEAD_BUFFER_BYTES)
        .http2_max_header_list_size(H2_HEADER_LIST_BYTES)
        .build(connector);
    Pool { client, connects }
}

/// Where an executor's connections live. A clone of a pooled transport
/// shares its pool.
#[derive(Clone)]
pub(crate) enum Transport {
    /// One pool for every session of the executor.
    /// Boxed, because a pool is much larger than a connector.
    Pooled(Box<Pool>),
    /// Each session builds its own pool from this connector.
    PerSession {
        connector: Connector,
        version: HttpVersion,
    },
}

/// Sends HTTP/1.1 and HTTP/2 requests, in plaintext or over TLS, through its
/// pool, or under `pool: vu` through the session's. [`Executors`] builds a
/// plan's executors, whose pools it may share. [`Executor::new`] builds a
/// lone one, whose pool is its own.
///
/// `send` must run inside a Tokio runtime, of either kind, because the client
/// spawns its connection tasks on it. Building needs no runtime.
///
/// How the [`HttpSettings`] are read:
/// - `version` sets the version mode: `auto` negotiates over TLS, `http1`
///   speaks only HTTP/1.1, and `http2` speaks only HTTP/2, with prior
///   knowledge in plaintext. Under `http2`, one hop per origin connects at a
///   time and the others wait for it. When that connect fails, the waiting
///   hops report its kind and text, found in the pool's log of its last 8
///   connects, and never another connect's failure. They keep `connect` and
///   "the shared connection attempt failed" when the connect was abandoned,
///   when it failed in HTTP/2's own handshake, when 8 newer connects pushed
///   it out of the log, when it started or failed during the hop's first
///   poll, a window of microseconds, and when hyper-util sent the hop back to
///   wait again, because the connection it got closed, or sent GOAWAY,
///   before its request went out. Under `auto` over TLS, a connect that ALPN turns into HTTP/2
///   can release hops that were running their own connects. They keep
///   `connect` too, since only `http2` connects are logged.
/// - `max_response_size` is the body limit. A longer body is read to the end,
///   its start kept up to the limit, and marked `truncated`. Its connection
///   is reused.
/// - `decompress` asks for gzip and br bodies and decodes them. A request
///   with no `Accept-Encoding` of its own gets `Accept-Encoding: gzip, br`. A
///   response whose one `Content-Encoding` is `gzip`, `x-gzip` or `br` is
///   decoded as it arrives, and the limit counts decoded bytes: decoding
///   stops there, and the rest is read but not decoded or checked. Decoding
///   runs in steps with a yield after each, so `timeout` covers it. A step
///   hands back at most 64 KiB of output, but a br step can decode up to
///   the stream's window, at most 16 MiB, first, because brotli writes its
///   ring buffer out only when it fills. Any other body is kept raw, and the
///   headers are left as received. A corrupt compressed body fails as `protocol` in
///   `body`. `decompress` doesn't change pool sharing.
/// - `pool` says where connections live. Under `shared` and `scenario`, all
///   the sessions of an executor use its one pool, which [`Executors`] shares
///   with other executors under `shared`. Under `vu`, each session holds its
///   own pool, with its own TLS session cache so users never resume each
///   other's sessions, and its clones share both. Every pool closes a
///   connection that has been idle for 90 s. A hop takes an idle connection
///   from its pool if one is there when it starts. If none is, it opens a new
///   one while it waits, and takes whichever is ready first: the new one, or
///   one that comes back to the pool. A connection it doesn't take stays in
///   the pool, so a hop isn't promised the previous hop's HTTP/1.1
///   connection.
/// - `cookies`, with the limits' `max_cookies`, sets whether each session
///   has a cookie jar and its size: none under `cookies: false` or
///   `max_cookies: 0`. A session reads every `Set-Cookie` of a response
///   when its head arrives, on every status, and stores those the cookie
///   rules accept. It sends its cookies with each
///   request they match. `cookies` and `max_cookies` don't change pool
///   sharing, because sharing a pool never shares cookies.
/// - `timeout` and `redirects` are core's. Core passes the time left to each
///   `send`, and follows redirects itself. A hop that ends at or after its
///   deadline fails as `timeout`, even when its body was all read or it
///   failed another way on the way.
/// - `tls.ca_file` names a PEM file, resolved against the test file's
///   directory, whose certificates are trusted as well as the system's
///   roots. It is read once, while building. `tls.insecure` accepts any
///   certificate for any name, though the handshake's signatures are still
///   checked, and `tls.ca_file` is then not read.
///
/// [`Executors`]: crate::Executors
pub struct Executor {
    id: u64,
    transport: Transport,
    limit: u64,
    /// The size of each session's jar, or `None` for no jar.
    pub(crate) cookies: Option<NonZeroU32>,
    /// Whether to ask for gzip and br and decode them.
    decompress: bool,
}

/// The size of each session's jar under `settings` and `limits`, or `None`
/// when sessions keep no cookies: under `cookies: false` or
/// `max_cookies: 0`.
pub(crate) fn jar_limit(settings: &HttpSettings, limits: &ResourceLimits) -> Option<NonZeroU32> {
    if settings.cookies {
        NonZeroU32::new(limits.max_cookies)
    } else {
        None
    }
}

impl Executor {
    /// A lone executor with a pool of its own, shared with no other. It
    /// trusts the system's root certificates, plus those in `tls.ca_file`,
    /// which is resolved against `test_dir`, the test file's directory.
    /// System roots that don't parse are skipped. Its background connects,
    /// and any HTTP/1.1 write that stalls, are bounded by `timeout`. Its jar
    /// limit is `limits.max_cookies`, and nothing else in `limits` is read.
    ///
    /// Building fails with [`BuildError::CaFile`] at [`Scope::File`] when
    /// `tls.ca_file` can't be used, and with [`BuildError::NoTrustedRoots`]
    /// when no root is left. Under `tls.insecure`, neither the system's roots
    /// nor `tls.ca_file` are read.
    pub fn new(
        settings: &HttpSettings,
        limits: &ResourceLimits,
        test_dir: &Path,
    ) -> Result<Self, BuildError> {
        Self::lone(settings, limits, test_dir, SystemRoots::Load)
    }

    /// Like [`Executor::new`], with `system` standing in for the system's
    /// roots. It never fails with [`BuildError::NoTrustedRoots`], so
    /// `RootCertStore::empty()` serves plaintext servers.
    pub fn with_roots(
        settings: &HttpSettings,
        limits: &ResourceLimits,
        test_dir: &Path,
        system: RootCertStore,
    ) -> Result<Self, BuildError> {
        Self::lone(settings, limits, test_dir, SystemRoots::StandIn(system))
    }

    /// Runs a plan's build steps for one scope at [`Scope::File`], because a
    /// lone executor's settings are file-shaped. It has no step lists, so
    /// its connect limit is `timeout`.
    fn lone(
        settings: &HttpSettings,
        limits: &ResourceLimits,
        test_dir: &Path,
        system: SystemRoots,
    ) -> Result<Self, BuildError> {
        let view = ScopeView {
            at: Scope::File,
            settings,
            limits,
            lists: Vec::new(),
        };
        let mut built = build(&[view], test_dir, system)?;
        Ok(built.pop().expect("one scope builds one executor"))
    }

    /// An executor over `transport`, with the body limit `max_response_size`,
    /// jars of `cookies` cookies, or none, and decoding under `decompress`.
    /// It gets an ID of its own.
    pub(crate) fn from_parts(
        transport: Transport,
        max_response_size: u64,
        cookies: Option<NonZeroU32>,
        decompress: bool,
    ) -> Self {
        Self {
            id: NEXT_EXECUTOR.fetch_add(1, Ordering::Relaxed),
            transport,
            limit: max_response_size,
            cookies,
            decompress,
        }
    }

    /// The pool a session's requests go through: its own, `own`, under
    /// `pool: vu`, the executor's otherwise. It takes the session's pool
    /// alone, so the session's jar can be borrowed beside it.
    fn pool_of<'a>(&'a self, own: Option<&'a Pool>) -> &'a Pool {
        match &self.transport {
            Transport::Pooled(pool) => pool,
            // The owner check passed, so this executor made the session, and
            // it gave the session a pool.
            Transport::PerSession { .. } => own.unwrap_or_else(|| {
                unreachable!("a session of a per-session executor holds a pool")
            }),
        }
    }

    /// Runs the rest of the hop until the hop's deadline, and reports a
    /// timeout in the stage its marker shows. The hop
    /// yields between decoding steps, so the deadline can fire while a body
    /// decodes. A hop that ends at or after its deadline, with `Ok` or a
    /// failure, still ran out of time, and reports a timeout too (see
    /// [`settle`]). With no deadline, the hop never runs out of time.
    async fn exchange(
        &self,
        pool: &Pool,
        mut request: Request<CountingBody>,
        capture: &CaptureConnection,
        deadline: Option<Instant>,
        progress: &mut Progress,
        jar: Option<(&mut Jar, &Target)>,
    ) -> Result<HttpResponse, Failure> {
        // `prepare` is done, so the hop is waiting for a connection: its own
        // connect, a pooled one, or, under `http2`, another hop's connect,
        // which hyper-util shares without running the connector for this hop.
        // Its request carries a mark that moves the marker to `headers` once
        // hyper gives it to a connection.
        let marker = Arc::new(StageMarker::new());
        marker.set(FailureStage::Connect);
        request
            .extensions_mut()
            .insert(Unwritten::new(marker.clone()));
        let hop = self.drive(pool, request, capture, &marker, progress, jar);
        let mut hop = std::pin::pin!(hop);
        let outcome = match deadline {
            None => hop.await,
            Some(deadline) => tokio::select! {
                biased;
                outcome = &mut hop => outcome,
                () = tokio::time::sleep_until(deadline.into()) => {
                    // Read before the hop is dropped: dropping it drops an
                    // unwritten request, whose mark moves the marker.
                    return Err(timeout(marker.get()));
                }
            },
        };
        // The deadline is a std Instant from core, so `now` is too (only tokio's pause differs).
        settle(outcome, deadline, Instant::now(), marker.get())
    }

    /// Sends the request, waits for its head, and reads its body up to the
    /// limit, decoding it under `decompress`. `jar` is the session's jar, if
    /// it has one, with the target the head's `Set-Cookie` values are stored
    /// against.
    ///
    /// A decoded body is decoded in steps, with a yield after each, so no
    /// poll runs more than one step: the hop's timeout can fire between
    /// steps, and other tasks on the worker run. A step hands back at most
    /// 64 KiB of output, but a br step can decode up to the stream's window,
    /// at most 16 MiB, first, because brotli writes its ring buffer out only
    /// when it fills. Each frame
    /// counts as received when it arrives, so a frame whose decoding a
    /// timeout cuts short still counts whole.
    async fn drive(
        &self,
        pool: &Pool,
        request: Request<CountingBody>,
        capture: &CaptureConnection,
        marker: &StageMarker,
        progress: &mut Progress,
        jar: Option<(&mut Jar, &Target)>,
    ) -> Result<HttpResponse, Failure> {
        let uri = request.uri().clone();
        let mut response = pool.client.request(request);
        // hyper-util makes a waiting hop a waiter in its first poll. The
        // pool's event count on both sides of that poll tells which connect
        // the hop waited on.
        let before = pool.connects.events();
        let first = poll_once(&mut response).await;
        let after = pool.connects.events();
        // hyper-util assigns the hop a connection, new or pooled, only inside
        // a poll of `response`. It assigns another when a reused connection
        // closed, or sent GOAWAY, before the request went out, so the hop
        // takes the setup of the connection it is on after every poll. Its
        // stage moves to `headers` on its own, when hyper gives its request
        // to a connection (see `Unwritten`). A connect that loses to a pooled
        // connection finishes on another task.
        claim(capture, progress);
        let head = match first {
            Poll::Ready(head) => head,
            Poll::Pending => {
                std::future::poll_fn(|cx| {
                    let polled = Pin::new(&mut response).poll(cx);
                    claim(capture, progress);
                    polled
                })
                .await
            }
        };
        // A failure before the head is in the marker's stage, unless its
        // cause places it.
        let head = match head {
            Ok(head) => head,
            Err(error) => {
                progress.error_connection = error.connect_info().and_then(conn_info);
                let failed = classify(&error, marker.get());
                // A hop that waited on another hop's connect learns why it
                // failed from the pool's log, when the log can tie a failure
                // to that connect. A hop that got a connection was sent back
                // to wait again, so the connect its first poll saw may not be
                // the one that released it, and it keeps `connect`.
                let got_connection = capture.connection_metadata().is_some();
                if is_shared_connect(&failed)
                    && !got_connection
                    && let Some(cause) = pool.connects.waited_on(&uri, before, after)
                {
                    return Err(connector_failure(cause));
                }
                return Err(failed);
            }
        };
        progress.to_headers = Some(progress.start.elapsed());
        marker.set(FailureStage::Body);
        // Stored before the body is read, on every status, so a hop whose
        // body fails or runs out of time keeps them. The response keeps its
        // `Set-Cookie` headers.
        if let Some((jar, target)) = jar {
            let now = SystemTime::now();
            for value in head.headers().get_all(SET_COOKIE) {
                jar.store(value.as_bytes(), target, now);
            }
        }

        let (parts, mut body) = head.into_parts();
        let version = wire_version(parts.version)
            .ok_or_else(|| failure(FailureKind::Protocol, FailureStage::Headers))?;
        // The encoding is known only now, from the head.
        let encoding = self
            .decompress
            .then(|| decode::encoding(&parts.headers))
            .flatten();
        let capture = progress.capture.insert(Capture::new(self.limit, encoding));
        // Every frame is read, so an HTTP/1.1 connection can be reused. A
        // corrupt compressed body fails at once.
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|error| classify(&error, FailureStage::Body))?;
            // Trailers are dropped.
            let Ok(data) = frame.into_data() else {
                continue;
            };
            capture.arrive(data.len());
            let mut rest = &data[..];
            while !rest.is_empty() {
                let stepped = capture.decodes();
                rest = &rest[capture.push(rest)?..];
                if stepped {
                    tokio::task::yield_now().await;
                }
            }
        }
        // A br stream can hold up to its window of output after its last
        // byte, and gzip its last buffer and trailer, so the end is decoded
        // in steps too.
        while !capture.drain()? {
            tokio::task::yield_now().await;
        }
        let (body, truncated) = capture.finish()?;
        let headers = parts
            .headers
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
            .collect();
        Ok(HttpResponse {
            status: parts.status.as_u16(),
            version,
            headers,
            body,
            truncated,
        })
    }
}

impl HttpExecutor for Executor {
    type Session = Session;

    fn new_session(&self) -> Session {
        let client = match &self.transport {
            Transport::Pooled(_) => None,
            Transport::PerSession { connector, version } => {
                Some(new_pool(connector.for_user(), *version))
            }
        };
        Session {
            owner: self.id,
            pool: client,
            jar: self.cookies.map(Jar::new),
        }
    }

    async fn send(
        &self,
        session: &mut Session,
        request: &HttpRequest,
        timeout_after: Duration,
    ) -> HttpHop {
        let mut progress = Progress::new();
        // A timeout too long to add to the start has no deadline.
        let deadline = progress.start.checked_add(timeout_after);
        let sent = Arc::new(AtomicU64::new(0));
        let mut capture = None;
        let outcome = if session.owner != self.id {
            Err(Failure {
                kind: FailureKind::InvalidRequest,
                stage: FailureStage::Prepare,
                message: text::FOREIGN_SESSION,
            })
        } else if timeout_after.is_zero() {
            Err(timeout(FailureStage::Prepare))
        } else {
            // The pool is borrowed apart from the jar, which the hop changes.
            let Session { pool, jar, .. } = session;
            match ready(request, jar.as_mut(), deadline, self.decompress) {
                Err(failure) => Err(failure),
                Ok(prepared) => {
                    let (mut request, target) = wire_request(prepared, sent.clone());
                    let capture = capture.insert(capture_connection(&mut request));
                    self.exchange(
                        self.pool_of(pool.as_ref()),
                        request,
                        capture,
                        deadline,
                        &mut progress,
                        jar.as_mut().map(|jar| (jar, &target)),
                    )
                    .await
                }
            }
        };

        // A failure in `connect` names no connection: none carried the hop's
        // request. Its capture may still name one it was queued on or handed
        // back from.
        let connecting = matches!(&outcome, Err(failure) if failure.stage == FailureStage::Connect);
        let info = if connecting {
            None
        } else {
            capture
                .as_ref()
                .and_then(|capture| capture.connection_metadata().as_ref().and_then(conn_info))
                .or(progress.error_connection)
        };
        let connection = info.as_ref().map(|info| info.id);
        // The setup the hop took for the connection it reports. Otherwise
        // take that connection's setup now, if any is left.
        let setup = match progress.setup.take() {
            Some((id, setup)) if Some(id) == connection => Some(setup),
            _ => info.and_then(|info| info.take_setup()),
        };
        let received = progress.capture.as_ref().map_or(0, Capture::received);
        HttpHop {
            timing: HopTiming {
                to_headers: progress.to_headers,
                total: progress.start.elapsed(),
                setup,
            },
            connection,
            sizes: BodySizes {
                sent: sent.load(Ordering::Relaxed),
                received,
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
    /// The body being read, from the moment the head arrives. Its count is
    /// what the hop received, on a failure too.
    capture: Option<Capture>,
    /// The connection a hyper-util error names, if any.
    error_connection: Option<Arc<ConnInfo>>,
    /// The setup the hop took from the connection it is on, with that
    /// connection's ID. It is taken after each poll that assigns a
    /// connection, not at the end, so a hop dropped after it got its
    /// connection, or after hyper-util moved it to another, takes the setup
    /// with it.
    setup: Option<(ConnectionId, ConnectionSetup)>,
}

impl Progress {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            to_headers: None,
            capture: None,
            error_connection: None,
            setup: None,
        }
    }

    /// Takes the setup of the connection `capture` names, unless the hop
    /// already holds that connection's. The setup of a connection the hop
    /// was moved off is dropped. A pooled connection has none left, and then
    /// the hop holds none.
    fn take_setup_from(&mut self, capture: &CaptureConnection) {
        let Some(info) = capture.connection_metadata().as_ref().and_then(conn_info) else {
            return;
        };
        if self.setup.as_ref().is_some_and(|(id, _)| *id == info.id) {
            return;
        }
        self.setup = info.take_setup().map(|setup| (info.id, setup));
    }
}

/// Everything a hop does before the wire, in order: `prepare`, adding the
/// jar's cookies, adding `Accept-Encoding` under `decompress`, the deadline
/// check, and counting those cookies as used. A failure in any step is in
/// `prepare`, and changes nothing in the jar but the expired cookies that
/// picking removes.
fn ready(
    request: &HttpRequest,
    mut jar: Option<&mut Jar>,
    deadline: Option<Instant>,
    decompress: bool,
) -> Result<Prepared, Failure> {
    let mut prepared = prepare(request)?;
    let picked = jar.as_deref_mut().and_then(|jar| {
        let own: Vec<&[u8]> = prepared
            .headers
            .get_all(COOKIE)
            .iter()
            .map(HeaderValue::as_bytes)
            .collect();
        jar.pick(&prepared.target, &own, SystemTime::now())
    });
    if let Some(picked) = &picked {
        add_cookies(&mut prepared, picked.pairs())?;
    }
    if decompress {
        add_accept_encoding(&mut prepared);
    }
    // Preparing can't be cut short, so the deadline is checked once it is
    // done.
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(timeout(FailureStage::Prepare));
    }
    if let (Some(jar), Some(picked)) = (jar, &picked) {
        jar.mark_used(picked);
    }
    Ok(prepared)
}

/// The request for the wire, and the target its response's cookies are
/// stored against.
fn wire_request(prepared: Prepared, sent: Arc<AtomicU64>) -> (Request<CountingBody>, Target) {
    let mut request = Request::new(CountingBody::new(prepared.body, sent));
    *request.method_mut() = prepared.method;
    *request.uri_mut() = prepared.uri;
    *request.headers_mut() = prepared.headers;
    (request, prepared.target)
}

/// The outcome of a hop that ended at `now`. Any outcome reached at or after
/// the `deadline` ran out of time all the same. A failure becomes a timeout
/// in its own stage, which is the marker's unless its cause placed it, as a
/// connect failure is. `Ok` becomes a timeout in `stage`, the marker's.
/// Before the deadline, and with no deadline, nothing changes.
fn settle(
    outcome: Result<HttpResponse, Failure>,
    deadline: Option<Instant>,
    now: Instant,
    stage: FailureStage,
) -> Result<HttpResponse, Failure> {
    match outcome {
        Err(failure) if deadline.is_some_and(|deadline| now >= deadline) => {
            Err(timeout(failure.stage))
        }
        Ok(_) if deadline.is_some_and(|deadline| now >= deadline) => Err(timeout(stage)),
        outcome => outcome,
    }
}

/// Once hyper-util has assigned the hop a connection, the hop holds the setup
/// of the connection it is on now.
fn claim(capture: &CaptureConnection, progress: &mut Progress) {
    if capture.connection_metadata().is_some() {
        progress.take_setup_from(capture);
    }
}

/// Polls `future` once, with the caller's waker.
async fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut *future).poll(cx))).await
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

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use bytes::Bytes;
    use http::Response;
    use http::header::AUTHORIZATION;
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use rcgen::{CertificateParams, KeyPair, SanType};
    use rustls::HandshakeKind;
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use vush_core::plan::PoolScope;
    use vush_core::protocol::http::RequestUrl;
    use vush_core::runner::run_attempt;

    use super::*;
    use crate::connector::Resolver;
    use crate::tls::{Verification, client_config};

    const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const NAMED_TIMEOUT: Duration = Duration::from_secs(5);

    /// A pooled `auto` executor with a jar of 64 cookies. Its resolver maps
    /// every name to 127.0.0.1 on the asked port, so no real resolver is
    /// asked.
    fn named() -> Executor {
        resolving(Arc::new(|_, port| {
            Box::pin(async move { Ok(vec![SocketAddr::new(LOCALHOST, port)]) })
        }))
    }

    /// Like [`named`], with the resolver `resolver`.
    fn resolving(resolver: Resolver) -> Executor {
        let tls = client_config(
            Verification::Roots(RootCertStore::empty()),
            HttpVersion::Auto,
        );
        let connector = Connector::with_resolver(tls, HttpVersion::Auto, NAMED_TIMEOUT, resolver);
        Executor::from_parts(
            Transport::Pooled(Box::new(new_pool(connector, HttpVersion::Auto))),
            1_048_576,
            NonZeroU32::new(64),
            false,
        )
    }

    /// The values of `name` in `headers`, joined with `, `, or empty.
    fn joined(headers: &http::HeaderMap, name: http::HeaderName) -> Vec<u8> {
        let values: Vec<&[u8]> = headers.get_all(name).iter().map(|v| v.as_bytes()).collect();
        values.join(b", ".as_slice())
    }

    /// An HTTP/1.1 server on 127.0.0.1 that answers every host name. It
    /// binds first and passes its port to `routes`, so a `Location` can name
    /// it under another name, and gives the port. A listed path gets its
    /// route's status and headers, and any other path `200` with no extra
    /// headers. Every response carries `x-seen-cookie` and
    /// `x-seen-authorization`: the request's values of those headers, or
    /// empty.
    async fn cookie_server(
        routes: impl FnOnce(u16) -> Vec<(&'static str, u16, Vec<(&'static str, String)>)>,
    ) -> u16 {
        let listener = TcpListener::bind((LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes = Arc::new(routes(port));
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let routes = routes.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let routes = routes.clone();
                        async move {
                            let mut response = Response::builder()
                                .header("x-seen-cookie", joined(request.headers(), COOKIE))
                                .header(
                                    "x-seen-authorization",
                                    joined(request.headers(), AUTHORIZATION),
                                );
                            let path = request.uri().path();
                            if let Some((_, status, headers)) =
                                routes.iter().find(|(listed, ..)| *listed == path)
                            {
                                response = response.status(*status);
                                for (name, value) in headers {
                                    response = response.header(*name, value.as_str());
                                }
                            }
                            let response = response.body(Full::new(Bytes::new())).unwrap();
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tcp), service)
                        .await;
                });
            }
        });
        port
    }

    fn get(url: String, headers: &[(&str, &str)]) -> HttpRequest {
        HttpRequest {
            method: "GET".parse().unwrap(),
            url: RequestUrl::Absolute(url),
            query: vec![],
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: None,
        }
    }

    /// The value of the response header `name`.
    fn header(response: &HttpResponse, name: &str) -> String {
        let (_, value) = response
            .headers
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("the response has no {name}"));
        String::from_utf8(value.clone()).unwrap()
    }

    /// The `Cookie` and `Authorization` the server saw.
    fn seen(response: &HttpResponse) -> (String, String) {
        (
            header(response, "x-seen-cookie"),
            header(response, "x-seen-authorization"),
        )
    }

    /// Sends a `GET` to `url` on `session` and gives the `Cookie` the server
    /// saw.
    async fn cookie_seen(executor: &Executor, session: &mut Session, url: String) -> String {
        let hop = executor.send(session, &get(url, &[]), NAMED_TIMEOUT).await;
        seen(&hop.outcome.expect("the hop should succeed")).0
    }

    /// The second of two hops in a row takes the first one's connection. The
    /// resolver answers only the first lookup, so a second connect can't
    /// finish and only the pooled connection can serve the hop.
    #[tokio::test(flavor = "multi_thread")]
    async fn plaintext_hops_in_a_row_share_a_connection() {
        let port = cookie_server(|_| vec![]).await;
        let answered = AtomicBool::new(false);
        let executor = resolving(Arc::new(move |_, port| {
            let first = !answered.swap(true, Ordering::SeqCst);
            Box::pin(async move {
                if first {
                    Ok(vec![SocketAddr::new(LOCALHOST, port)])
                } else {
                    std::future::pending().await
                }
            })
        }));
        let mut session = executor.new_session();
        let request = get(format!("http://reuse.vush.test:{port}/"), &[]);
        let first = executor.send(&mut session, &request, NAMED_TIMEOUT).await;
        first.outcome.as_ref().expect("the first hop succeeds");
        let second = executor.send(&mut session, &request, NAMED_TIMEOUT).await;
        second.outcome.as_ref().expect("the second hop succeeds");
        assert!(first.timing.setup.is_some());
        assert!(second.timing.setup.is_none());
        assert!(first.connection.is_some());
        assert_eq!(first.connection, second.connection);
    }

    #[tokio::test]
    async fn domain_cookies_reach_hosts_under_the_domain() {
        let port = cookie_server(|_| {
            vec![(
                "/set",
                200,
                vec![
                    ("set-cookie", "h=1".to_owned()),
                    ("set-cookie", "d=1; Domain=vush.test".to_owned()),
                ],
            )]
        })
        .await;
        let executor = named();
        let mut session = executor.new_session();
        let url = |host: &str, path: &str| format!("http://{host}:{port}{path}");
        assert_eq!(
            cookie_seen(&executor, &mut session, url("a.vush.test", "/set")).await,
            ""
        );
        assert_eq!(
            cookie_seen(&executor, &mut session, url("b.vush.test", "/")).await,
            "d=1"
        );
        assert_eq!(
            cookie_seen(&executor, &mut session, url("other.test", "/")).await,
            ""
        );
        assert_eq!(
            cookie_seen(&executor, &mut session, url("a.vush.test", "/")).await,
            "h=1; d=1"
        );
    }

    /// Keeps every response its executor gets, so a test sees the hops that
    /// `run_attempt` drops.
    struct Recording {
        inner: Executor,
        responses: Mutex<Vec<HttpResponse>>,
    }

    impl HttpExecutor for Recording {
        type Session = Session;

        fn new_session(&self) -> Session {
            self.inner.new_session()
        }

        async fn send(
            &self,
            session: &mut Session,
            request: &HttpRequest,
            timeout: Duration,
        ) -> HttpHop {
            let hop = self.inner.send(session, request, timeout).await;
            if let Ok(response) = &hop.outcome {
                self.responses.lock().unwrap().push(response.clone());
            }
            hop
        }

        fn follow(
            &self,
            request: &HttpRequest,
            response: &HttpResponse,
        ) -> Result<Option<HttpRequest>, Failure> {
            self.inner.follow(request, response)
        }
    }

    #[tokio::test]
    async fn a_cross_host_redirect_carries_only_the_new_hosts_cookies() {
        let port = cookie_server(|port| {
            vec![
                (
                    "/start",
                    302,
                    vec![
                        ("set-cookie", "jar=1".to_owned()),
                        ("location", format!("http://other.test:{port}/back")),
                    ],
                ),
                (
                    "/back",
                    302,
                    vec![("location", format!("http://a.vush.test:{port}/end"))],
                ),
            ]
        })
        .await;
        let executor = Recording {
            inner: named(),
            responses: Mutex::new(Vec::new()),
        };
        let mut session = executor.new_session();
        let request = get(
            format!("http://a.vush.test:{port}/start"),
            &[("Cookie", "mine=1"), ("Authorization", "Bearer t")],
        );
        let attempt = run_attempt(&executor, &mut session, request, NAMED_TIMEOUT, 5).await;
        assert_eq!(
            attempt.response.expect("the chain ends at /end").status,
            200
        );

        let responses = executor.responses.into_inner().unwrap();
        let statuses: Vec<u16> = responses.iter().map(|r| r.status).collect();
        assert_eq!(statuses, [302, 302, 200]);
        let want =
            |cookie: &str, authorization: &str| (cookie.to_owned(), authorization.to_owned());
        assert_eq!(seen(&responses[0]), want("mine=1", "Bearer t"), "/start");
        assert_eq!(seen(&responses[1]), want("", ""), "other.test/back");
        assert_eq!(seen(&responses[2]), want("jar=1", ""), "/end");
    }

    /// A TLS server for 127.0.0.1, and a root store that trusts it. It
    /// speaks HTTP/1.1, answers each request with `x-handshake: resumed` when
    /// its connection resumed a session and `x-handshake: full` otherwise,
    /// then closes the connection, so each request needs a new one. Like any
    /// rustls server, it sends resumption tickets after each handshake.
    async fn handshake_server() -> (SocketAddr, RootCertStore) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![SanType::IpAddress(LOCALHOST)];
        let cert = params.self_signed(&key).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind((LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let kind = match tls.get_ref().1.handshake_kind() {
                        Some(HandshakeKind::Resumed) => "resumed",
                        _ => "full",
                    };
                    let service = service_fn(move |_| async move {
                        let response = Response::builder()
                            .header("x-handshake", kind)
                            .body(Full::new(Bytes::new()))
                            .unwrap();
                        Ok::<_, Infallible>(response)
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .keep_alive(false)
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                });
            }
        });
        (addr, roots)
    }

    /// Sends `request` on `session` and gives the handshake its connection
    /// had, as the server saw it.
    async fn handshake(
        executor: &Executor,
        session: &mut Session,
        request: &HttpRequest,
    ) -> String {
        let hop = executor
            .send(session, request, Duration::from_secs(5))
            .await;
        let response = hop.outcome.expect("the hop should succeed");
        let (_, value) = response
            .headers
            .iter()
            .find(|(name, _)| name == "x-handshake")
            .expect("the server names the handshake");
        String::from_utf8(value.clone()).unwrap()
    }

    #[tokio::test]
    async fn vu_users_never_resume_each_others_tls_sessions() {
        let (addr, roots) = handshake_server().await;
        let settings = HttpSettings {
            pool: PoolScope::Vu,
            ..HttpSettings::default()
        };
        let executor =
            Executor::with_roots(&settings, &ResourceLimits::default(), Path::new("."), roots)
                .unwrap();
        let request = HttpRequest {
            method: "GET".parse().unwrap(),
            url: RequestUrl::Absolute(format!("https://{addr}/")),
            query: vec![],
            headers: vec![],
            body: None,
        };
        let mut one = executor.new_session();
        let mut two = executor.new_session();
        assert_eq!(handshake(&executor, &mut one, &request).await, "full");
        assert_eq!(handshake(&executor, &mut one, &request).await, "resumed");
        assert_eq!(handshake(&executor, &mut two, &request).await, "full");
    }

    #[test]
    fn ca_file_texts_name_the_key_path() {
        let cases = [
            (
                Scope::File,
                CaFileProblem::NotFound,
                "`http.tls.ca_file` can't be used: the file doesn't exist",
            ),
            (
                Scope::File,
                CaFileProblem::Unreadable,
                "`http.tls.ca_file` can't be used: the file can't be read",
            ),
            (
                Scope::File,
                CaFileProblem::TooLarge,
                "`http.tls.ca_file` can't be used: the file is larger than 1 MiB",
            ),
            (
                Scope::File,
                CaFileProblem::NoCertificates,
                "`http.tls.ca_file` can't be used: the file holds no PEM certificate",
            ),
            (
                Scope::File,
                CaFileProblem::BadCertificate,
                "`http.tls.ca_file` can't be used: the file holds a certificate that can't be used",
            ),
            (
                Scope::Scenario(3),
                CaFileProblem::NotFound,
                "`scenarios[3].http.tls.ca_file` can't be used: the file doesn't exist",
            ),
            (
                Scope::Scenario(3),
                CaFileProblem::Unreadable,
                "`scenarios[3].http.tls.ca_file` can't be used: the file can't be read",
            ),
            (
                Scope::Scenario(3),
                CaFileProblem::TooLarge,
                "`scenarios[3].http.tls.ca_file` can't be used: the file is larger than 1 MiB",
            ),
            (
                Scope::Scenario(3),
                CaFileProblem::NoCertificates,
                "`scenarios[3].http.tls.ca_file` can't be used: the file holds no PEM certificate",
            ),
            (
                Scope::Scenario(3),
                CaFileProblem::BadCertificate,
                "`scenarios[3].http.tls.ca_file` can't be used: the file holds a certificate that can't be used",
            ),
        ];
        for (at, problem, text) in cases {
            let error = BuildError::CaFile { at, problem };
            assert_eq!(error.to_string(), text, "{at:?} {problem:?}");
        }
        assert_eq!(
            BuildError::NoTrustedRoots.to_string(),
            "no trusted root certificates could be loaded from the system"
        );
    }

    #[test]
    fn an_outcome_at_or_after_the_deadline_is_a_timeout() {
        let ok = || {
            Ok(HttpResponse {
                status: 200,
                version: WireVersion::Http11,
                headers: vec![],
                body: b"done".to_vec(),
                truncated: false,
            })
        };
        let now = Instant::now();
        let later = now + Duration::from_secs(1);
        let body = FailureStage::Body;
        let headers = FailureStage::Headers;
        let protocol = failure(FailureKind::Protocol, body);
        assert_eq!(settle(ok(), Some(later), now, body), ok(), "ok before");
        assert_eq!(
            settle(Err(protocol), Some(later), now, body),
            Err(protocol),
            "err before"
        );
        assert_eq!(
            settle(ok(), Some(now), now, body),
            Err(timeout(body)),
            "ok at"
        );
        assert_eq!(
            settle(Err(protocol), Some(now), now, body),
            Err(timeout(body)),
            "err at"
        );
        assert_eq!(
            settle(ok(), Some(now), later, headers),
            Err(timeout(headers)),
            "ok after, in the stage given"
        );
        assert_eq!(
            settle(Err(protocol), Some(now), later, headers),
            Err(timeout(body)),
            "err after, in the error's own stage"
        );
        let connect = failure(FailureKind::Connect, FailureStage::Connect);
        assert_eq!(
            settle(Err(connect), Some(now), later, headers),
            Err(timeout(FailureStage::Connect)),
            "a connect failure after, in connect"
        );
        assert_eq!(settle(ok(), None, later, body), ok(), "ok, no deadline");
        assert_eq!(
            settle(Err(protocol), None, later, body),
            Err(protocol),
            "err, no deadline"
        );
    }

    #[test]
    fn an_empty_jar_leaves_the_requests_own_cookies_as_written() {
        let mut jar = Jar::new(NonZeroU32::new(8).unwrap());
        let request = get(
            "http://h.test/".to_owned(),
            &[("Cookie", "a=1"), ("Cookie", "b=2")],
        );
        let prepared = ready(&request, Some(&mut jar), None, false).unwrap();
        let values: Vec<&[u8]> = prepared
            .headers
            .get_all(COOKIE)
            .iter()
            .map(HeaderValue::as_bytes)
            .collect();
        assert_eq!(values, [b"a=1".as_slice(), b"b=2".as_slice()]);
    }

    /// A lookup that fails fails the hop as `dns` at connect, before any
    /// connection or time to headers exists. A test through the public
    /// executor can't cause it, because it needs the private resolver hook.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_lookup_fails_the_hop_as_dns() {
        let executor = resolving(Arc::new(|_, _| {
            Box::pin(async { Err(std::io::Error::other("no such host")) })
        }));
        let mut session = executor.new_session();
        let request = get("http://missing.vush.test/".to_owned(), &[]);
        let hop = executor.send(&mut session, &request, NAMED_TIMEOUT).await;
        assert_eq!(
            hop.outcome.as_ref().unwrap_err(),
            &Failure {
                kind: FailureKind::Dns,
                stage: FailureStage::Connect,
                message: "the host name didn't resolve",
            }
        );
        assert_eq!(hop.connection, None);
        assert_eq!(hop.timing.setup, None);
        assert_eq!(hop.timing.to_headers, None);
    }

    /// A named host's connection setup includes its lookup time.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_named_host_times_its_lookup() {
        let port = cookie_server(|_| vec![]).await;
        let executor = named();
        let mut session = executor.new_session();
        let request = get(format!("http://api.vush.test:{port}/"), &[]);
        let hop = executor.send(&mut session, &request, NAMED_TIMEOUT).await;
        hop.outcome.as_ref().expect("the hop should succeed");
        let setup = hop.timing.setup.expect("a new connection");
        assert!(setup.dns.is_some());
    }

    /// A pooled `http2` executor whose first lookup never ends and whose
    /// later lookups all fail at once. Its connector gives up on a connect
    /// after `limit`.
    fn flaky_http2(limit: Duration) -> Executor {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resolver: Resolver = Arc::new(move |_, _| {
            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                if first {
                    std::future::pending().await
                } else {
                    Err(std::io::Error::other("no such host"))
                }
            })
        });
        let tls = client_config(
            Verification::Roots(RootCertStore::empty()),
            HttpVersion::Http2,
        );
        let connector = Connector::with_resolver(tls, HttpVersion::Http2, limit, resolver);
        Executor::from_parts(
            Transport::Pooled(Box::new(new_pool(connector, HttpVersion::Http2))),
            1_048_576,
            NonZeroU32::new(64),
            false,
        )
    }

    /// The waiting hop is woken when the shared connect fails, but it isn't
    /// polled until after a later connect for the same origin has failed
    /// too. It still reports the connect it waited on.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_later_failure_doesnt_replace_the_one_a_hop_waited_on() {
        let executor = flaky_http2(Duration::from_millis(100));
        let request = get("http://flaky.vush.test/".to_owned(), &[]);
        let (mut s1, mut s2, mut s3) = (
            executor.new_session(),
            executor.new_session(),
            executor.new_session(),
        );
        // Its connect waits on the lookup that never ends.
        let mut connecting = Box::pin(executor.send(&mut s1, &request, NAMED_TIMEOUT));
        assert!(poll_once(&mut connecting).await.is_pending());
        // A waiter, because `connecting` holds the connecting lock.
        let mut waiting = Box::pin(executor.send(&mut s2, &request, NAMED_TIMEOUT));
        assert!(poll_once(&mut waiting).await.is_pending());

        let first = connecting.await;
        let timed_out = Failure {
            kind: FailureKind::Timeout,
            stage: FailureStage::Connect,
            message: "the connection took too long",
        };
        assert_eq!(first.outcome.as_ref().unwrap_err(), &timed_out);
        assert_eq!(first.connection, None);

        // This hop takes the free lock, and its lookup fails at once.
        let later = executor.send(&mut s3, &request, NAMED_TIMEOUT).await;
        assert_eq!(
            later.outcome.as_ref().unwrap_err(),
            &Failure {
                kind: FailureKind::Dns,
                stage: FailureStage::Connect,
                message: "the host name didn't resolve",
            }
        );

        let waited = waiting.await;
        assert_eq!(waited.outcome.as_ref().unwrap_err(), &timed_out);
        assert_eq!(waited.connection, None);
    }

    /// The first connect is abandoned while a hop waits on it, which logs no
    /// failure. A third hop then takes the free lock and fails as `dns`. The
    /// waiting hop is woken but not polled until after that, and it keeps
    /// `connect`, not the `dns` of a connect it didn't wait on.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hop_whose_connect_was_abandoned_doesnt_take_a_later_failure() {
        let executor = flaky_http2(Duration::from_secs(60));
        let request = get("http://flaky.vush.test/".to_owned(), &[]);
        let (mut s1, mut s2, mut s3) = (
            executor.new_session(),
            executor.new_session(),
            executor.new_session(),
        );
        let mut connecting = Box::pin(executor.send(&mut s1, &request, NAMED_TIMEOUT));
        assert!(poll_once(&mut connecting).await.is_pending());
        let mut waiting = Box::pin(executor.send(&mut s2, &request, NAMED_TIMEOUT));
        assert!(poll_once(&mut waiting).await.is_pending());
        // The connect ends by being dropped, and logs nothing.
        drop(connecting);

        let later = executor.send(&mut s3, &request, NAMED_TIMEOUT).await;
        assert_eq!(
            later.outcome.as_ref().unwrap_err(),
            &Failure {
                kind: FailureKind::Dns,
                stage: FailureStage::Connect,
                message: "the host name didn't resolve",
            }
        );

        let waited = waiting.await;
        assert_eq!(
            waited.outcome.as_ref().unwrap_err(),
            &Failure {
                kind: FailureKind::Connect,
                stage: FailureStage::Connect,
                message: "the shared connection attempt failed",
            }
        );
        assert_eq!(waited.connection, None);
    }
}
