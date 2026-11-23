//! Fixture servers inside the test process, on 127.0.0.1 only, and helpers
//! to build executors, plans and requests.
//!
//! Servers stay up until the test's runtime ends. A server that stalls awaits
//! `std::future::pending()`, so no test races a sleep.

use std::convert::Infallible;
use std::error::Error as StdError;
use std::future::{Future, pending};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use vush_core::plan::{
    ByteSize, ClosedStop, ClosedVusLoad, Expect, HttpMethod, HttpSettings, HttpVersion, JsonNumber,
    LoadModel, PlanDraft, RequestStep, RequestTarget, ResourceLimits, Scenario, StatePolicy, Step,
    WorkloadPlan,
};
use vush_core::protocol::JsonValue;
use vush_core::protocol::http::{
    HttpBody, HttpExecutor, HttpHop, HttpRequest, HttpResponse, RequestUrl,
};
use vush_core::protocol::{FailureKind, FailureStage};
use vush_http::Executor;

pub const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
pub const MIB: u64 = 1_048_576;

// ---- Gated reuse ----
//
// hyper-util returns an HTTP/1.1 connection to its pool from a task of its
// own, so a hop that starts right after another races a new connect
// against that return.
//
// A test that asserts two hops share a connection makes the race
// impossible. Its server answers only as many TLS handshakes as
// connections the test expects, so an extra connect waits in its
// handshake and never finishes, and only the pooled connection can serve
// the hop. Count the connections the test opens, and open them all before
// its first reuse hop.
//
// HTTP/2 isn't affected, because hyper-util shares its connection at once.

/// The hop timeout of every test that doesn't state its own.
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// The test directory of executors that set no `tls.ca_file`.
pub const NO_DIR: &str = ".";

pub type BoxError = Box<dyn StdError + Send + Sync>;
pub type ReplyBody = BoxBody<Bytes, BoxError>;
pub type Reply = Response<ReplyBody>;
pub type ReplyFuture = Pin<Box<dyn Future<Output = Reply> + Send>>;
/// What a raw server does with one accepted connection.
pub type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

type Handler = Arc<dyn Fn(Request<Incoming>) -> ReplyFuture + Send + Sync>;

// ---- Executors, plans, requests and hops ----

pub fn settings(version: HttpVersion, limit: u64) -> HttpSettings {
    HttpSettings {
        version,
        max_response_size: ByteSize(limit),
        ..HttpSettings::default()
    }
}

/// An executor that trusts no certificate, for plaintext servers.
pub fn executor(version: HttpVersion) -> Executor {
    limited(version, MIB)
}

/// Like [`executor`], with the body limit `limit`.
pub fn limited(version: HttpVersion, limit: u64) -> Executor {
    let settings = settings(version, limit);
    Executor::with_roots(
        &settings,
        &ResourceLimits::default(),
        Path::new(NO_DIR),
        RootCertStore::empty(),
    )
    .unwrap()
}

/// An executor that trusts only `ca`.
pub fn trusting(ca: &TestCa, version: HttpVersion) -> Executor {
    Executor::with_roots(
        &settings(version, MIB),
        &ResourceLimits::default(),
        Path::new(NO_DIR),
        ca.roots(),
    )
    .unwrap()
}

/// A plan whose file level has the settings `file` and no hooks. Scenario
/// `i` is named `s<i>`, has `scenarios[i]` as its merged settings, and runs
/// one user once through one `GET` to `http://127.0.0.1/` with its
/// `http.timeout`.
pub fn plan(file: HttpSettings, scenarios: Vec<HttpSettings>) -> WorkloadPlan {
    let scenarios = scenarios
        .into_iter()
        .enumerate()
        .map(|(index, http)| {
            let request = RequestStep {
                name: None,
                method: method("GET"),
                target: RequestTarget::Url("http://127.0.0.1/".parse().unwrap()),
                query: vec![],
                headers: vec![],
                body: None,
                timeout: http.timeout,
                required: true,
                retry: None,
                expect: Expect::default(),
                extract: vec![],
            };
            Scenario {
                name: format!("s{index}"),
                load: LoadModel::ClosedVus(ClosedVusLoad::Fixed {
                    vus: NonZeroU32::MIN,
                    stop: ClosedStop::IterationsPerVu(NonZeroU64::MIN),
                }),
                http,
                limits: ResourceLimits::default(),
                max_request_rate: None,
                state: StatePolicy::Persist,
                setup: vec![],
                teardown: vec![],
                vu_setup: vec![],
                vu_teardown: vec![],
                steps: vec![Step::Request(Box::new(request))],
            }
        })
        .collect();
    let draft = PlanDraft {
        name: "pools".to_owned(),
        base_url: None,
        http: file,
        limits: ResourceLimits::default(),
        max_request_rate: None,
        setup: vec![],
        teardown: vec![],
        thresholds: vec![],
        scenarios,
    };
    WorkloadPlan::new(draft).unwrap()
}

pub fn method(name: &str) -> HttpMethod {
    name.parse().unwrap()
}

pub fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

pub fn get(url: String) -> HttpRequest {
    HttpRequest {
        method: method("GET"),
        url: RequestUrl::Absolute(url),
        query: vec![],
        headers: vec![],
        body: None,
    }
}

/// `{"a":1}` as a JSON body.
pub fn json_a_1() -> HttpBody {
    HttpBody::Json(JsonValue::Object(vec![(
        "a".to_owned(),
        JsonValue::Number(JsonNumber::Int(1)),
    )]))
}

/// Sends `request` on a fresh session, within [`TIMEOUT`].
pub async fn send(executor: &Executor, request: &HttpRequest) -> HttpHop {
    send_within(executor, request, TIMEOUT).await
}

/// Polls `future` once and gives what that poll returned. The future keeps
/// its progress, so a test can poll it again or drop it.
pub async fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut *future).poll(cx))).await
}

pub async fn send_within(executor: &Executor, request: &HttpRequest, timeout: Duration) -> HttpHop {
    let mut session = executor.new_session();
    executor.send(&mut session, request, timeout).await
}

pub fn response(hop: &HttpHop) -> &HttpResponse {
    match &hop.outcome {
        Ok(response) => response,
        Err(failure) => panic!("the hop should succeed, but failed with {failure}"),
    }
}

pub fn failure(hop: &HttpHop) -> (FailureKind, FailureStage) {
    match &hop.outcome {
        Ok(response) => panic!("the hop should fail, but got {response:?}"),
        Err(failure) => (failure.kind, failure.stage),
    }
}

/// Every value of the response header `name`, in order.
pub fn header_values<'a>(response: &'a HttpResponse, name: &str) -> Vec<&'a [u8]> {
    response
        .headers
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, value)| value.as_slice())
        .collect()
}

/// What the echo handler saw.
pub struct Echo {
    pub method: String,
    /// The path with its query.
    pub target: String,
    /// Every header in arrival order, names in lowercase.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Echo {
    pub fn of(hop: &HttpHop) -> Self {
        Self::of_response(response(hop))
    }

    pub fn of_response(response: &HttpResponse) -> Self {
        let bytes = &response.body;
        let blank = bytes
            .windows(2)
            .position(|pair| pair == b"\n\n")
            .expect("an echo has a blank line after its head");
        let head = std::str::from_utf8(&bytes[..blank]).expect("the echoed head is UTF-8");
        let mut lines = head.split('\n');
        let method = lines.next().unwrap().to_owned();
        let target = lines.next().unwrap().to_owned();
        let headers = lines
            .map(|line| {
                let (name, value) = line.split_once(": ").expect("a header line");
                (name.to_owned(), value.to_owned())
            })
            .collect();
        Self {
            method,
            target,
            headers,
            body: bytes[blank + 2..].to_vec(),
        }
    }

    /// Every value of the header `name`, in arrival order.
    pub fn values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

// ---- Certificates ----

/// A certificate authority made for one test.
pub struct TestCa(CertifiedIssuer<'static, KeyPair>);

/// A leaf certificate and its key.
pub struct Leaf {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

impl TestCa {
    pub fn generate() -> Self {
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "vush test ca");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        Self(CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap())
    }

    /// A root store that holds only this CA.
    pub fn roots(&self) -> RootCertStore {
        let mut roots = RootCertStore::empty();
        roots.add(self.0.der().clone()).unwrap();
        roots
    }

    /// This CA's certificate in PEM.
    pub fn pem(&self) -> String {
        self.0.pem()
    }

    /// A leaf for the IP address `ip`, so no name is ever resolved.
    pub fn leaf_for_ip(&self, ip: IpAddr) -> Leaf {
        self.leaf(SanType::IpAddress(ip))
    }

    /// A leaf for the DNS name `name` only.
    pub fn leaf_for_name(&self, name: &str) -> Leaf {
        self.leaf(SanType::DnsName(name.try_into().unwrap()))
    }

    fn leaf(&self, san: SanType) -> Leaf {
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "vush test leaf");
        params.subject_alt_names = vec![san];
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.0).unwrap();
        Leaf {
            cert: cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        }
    }
}

// ---- Files ----

/// A directory of its own under the system's temp dir, named from the
/// process ID and a counter, and removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("vush-http-test-{}-{n}", std::process::id());
        let path = std::env::temp_dir().join(name);
        // A directory left by a process that had the same ID.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Writes `contents` to the file `name` in this directory, and gives its
    /// path.
    pub fn write(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---- HTTP servers ----

async fn bind() -> TcpListener {
    TcpListener::bind((LOCALHOST, 0)).await.unwrap()
}

fn boxed<H, F>(handler: H) -> Handler
where
    H: Fn(Request<Incoming>) -> F + Send + Sync + 'static,
    F: Future<Output = Reply> + Send + 'static,
{
    Arc::new(move |request| Box::pin(handler(request)) as ReplyFuture)
}

/// Serves HTTP/1.1 only, in plaintext.
pub async fn spawn_h1<H, F>(handler: H) -> SocketAddr
where
    H: Fn(Request<Incoming>) -> F + Send + Sync + 'static,
    F: Future<Output = Reply> + Send + 'static,
{
    let handler = boxed(handler);
    let listener = bind().await;
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let reply = handler(request);
                    async move { Ok::<_, Infallible>(reply.await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tcp), service)
                    .await;
            });
        }
    });
    addr
}

/// Serves HTTP/1.1, or HTTP/2 when the client opens with its preface.
async fn serve_auto<I>(io: I, handler: Handler)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let service = service_fn(move |request| {
        let reply = handler(request);
        async move { Ok::<_, Infallible>(reply.await) }
    });
    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
        .serve_connection(io, service)
        .await;
}

/// Serves plaintext HTTP/1.1 and HTTP/2 with prior knowledge (h2c).
pub async fn spawn_auto<H, F>(handler: H) -> SocketAddr
where
    H: Fn(Request<Incoming>) -> F + Send + Sync + 'static,
    F: Future<Output = Reply> + Send + 'static,
{
    let handler = boxed(handler);
    let listener = bind().await;
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(serve_auto(TokioIo::new(tcp), handler.clone()));
        }
    });
    addr
}

/// Serves over TLS with `leaf`, choosing from `alpn` the way rustls does.
///
/// With `max_handshakes: Some(n)`, only the first `n` connections get a
/// handshake. Later ones are held open and never answered, so a client that
/// opens one can only get a response on a connection it already has.
pub async fn spawn_tls<H, F>(
    handler: H,
    alpn: &[&[u8]],
    max_handshakes: Option<usize>,
    leaf: Leaf,
) -> SocketAddr
where
    H: Fn(Request<Incoming>) -> F + Send + Sync + 'static,
    F: Future<Output = Reply> + Send + 'static,
{
    let handler = boxed(handler);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![leaf.cert], leaf.key)
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|name| name.to_vec()).collect();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = bind().await;
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut accepted = 0;
        while let Ok((tcp, _)) = listener.accept().await {
            accepted += 1;
            if max_handshakes.is_some_and(|max| accepted > max) {
                tokio::spawn(hold(tcp));
                continue;
            }
            let acceptor = acceptor.clone();
            let handler = handler.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    serve_auto(TokioIo::new(tls), handler).await;
                }
            });
        }
    });
    addr
}

/// Serves HTTP/1.1 over TLS and answers only `handshakes` handshakes, so a
/// test can count on its connections being the only ones.
pub async fn spawn_gated_h1<H, F>(handler: H, ca: &TestCa, handshakes: usize) -> SocketAddr
where
    H: Fn(Request<Incoming>) -> F + Send + Sync + 'static,
    F: Future<Output = Reply> + Send + 'static,
{
    spawn_tls(
        handler,
        &[b"http/1.1"],
        Some(handshakes),
        ca.leaf_for_ip(LOCALHOST),
    )
    .await
}

// ---- Handlers ----

pub fn full(body: impl Into<Bytes>) -> ReplyBody {
    Full::new(body.into())
        .map_err(|never| match never {})
        .boxed()
}

pub fn reply(status: u16, headers: &[(&str, &str)], body: impl Into<Bytes>) -> Reply {
    let mut response = Response::new(full(body));
    *response.status_mut() = StatusCode::from_u16(status).unwrap();
    for (name, value) in headers {
        response.headers_mut().append(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    response
}

/// Answers `200` with what arrived, one item per line: the method, the path
/// with its query, each header as `name: value` in arrival order, then a
/// blank line and the body. [`Echo::of`] reads it back.
pub fn echo(request: Request<Incoming>) -> ReplyFuture {
    Box::pin(async move {
        let (parts, body) = request.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        let mut out = Vec::new();
        out.extend_from_slice(parts.method.as_str().as_bytes());
        out.push(b'\n');
        let target = parts
            .uri
            .path_and_query()
            .map_or("/", |target| target.as_str());
        out.extend_from_slice(target.as_bytes());
        out.push(b'\n');
        for (name, value) in &parts.headers {
            out.extend_from_slice(name.as_str().as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value.as_bytes());
            out.push(b'\n');
        }
        out.push(b'\n');
        out.extend_from_slice(&body);
        reply(200, &[], out)
    })
}

/// Answers each path in `routes` with its status and `Location`, after
/// reading the request body, and echoes every other path.
pub fn redirecting(
    routes: &[(&str, u16, &str)],
) -> impl Fn(Request<Incoming>) -> ReplyFuture + Send + Sync + 'static {
    let routes: Vec<(String, u16, String)> = routes
        .iter()
        .map(|(path, status, location)| ((*path).to_owned(), *status, (*location).to_owned()))
        .collect();
    move |request| {
        let route = routes
            .iter()
            .find(|(path, _, _)| path == request.uri().path())
            .cloned();
        match route {
            None => echo(request),
            Some((_, status, location)) => Box::pin(async move {
                let _ = request.into_body().collect().await;
                reply(status, &[("location", &location)], "")
            }),
        }
    }
}

/// A body of `left` bytes sent in chunks, with no length known up front.
struct Chunks {
    left: usize,
}

impl hyper::body::Body for Chunks {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if self.left == 0 {
            return Poll::Ready(None);
        }
        let size = self.left.min(64 * 1024);
        self.left -= size;
        Poll::Ready(Some(Ok(Frame::data(Bytes::from(vec![b'x'; size])))))
    }

    fn is_end_stream(&self) -> bool {
        self.left == 0
    }
}

/// Answers `/fixed/{n}` with `n` bytes and a `content-length`, and
/// `/chunked/{n}` with `n` bytes in chunks. Both carry `x-size: {n}`.
pub fn sized(request: Request<Incoming>) -> ReplyFuture {
    let path = request.uri().path().to_owned();
    Box::pin(async move {
        let (kind, size) = path
            .trim_start_matches('/')
            .split_once('/')
            .expect("/fixed/{n} or /chunked/{n}");
        let size: usize = size.parse().unwrap();
        let body = match kind {
            "fixed" => full(vec![b'x'; size]),
            _ => Chunks { left: size }.boxed(),
        };
        let mut response = Response::new(body);
        response.headers_mut().insert("x-size", size.into());
        response
    })
}

// ---- Raw TCP servers ----

/// Runs `behavior` on every connection it accepts.
pub async fn spawn_raw(behavior: fn(TcpStream) -> BoxFuture) -> SocketAddr {
    let listener = bind().await;
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(behavior(tcp));
        }
    });
    addr
}

/// A port on 127.0.0.1 that refuses every connection at once. It is the
/// client end of a loopback connection that stays open while the guard
/// lives. Nothing listens on it, so a connect gets a reset, and its socket
/// keeps other tests from binding the port. A socket that is only bound
/// won't do: Linux refuses a connect to it, but macOS drops it unanswered.
pub struct RefusedPort {
    _client: std::net::TcpStream,
    _server: std::net::TcpStream,
    pub addr: SocketAddr,
}

pub fn refused_port() -> RefusedPort {
    let listener = std::net::TcpListener::bind((LOCALHOST, 0)).unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    let addr = client.local_addr().unwrap();
    RefusedPort {
        _client: client,
        _server: server,
        addr,
    }
}

/// A listener that never answers. A test can take each connection as it
/// arrives, or count the connections that reached it.
pub struct CountingListener {
    listener: TcpListener,
    pub addr: SocketAddr,
}

impl CountingListener {
    pub async fn bind() -> Self {
        let listener = bind().await;
        let addr = listener.local_addr().unwrap();
        Self { listener, addr }
    }

    /// Waits for the next connection and hands it over, so the test knows it
    /// arrived and can hold it open.
    pub async fn arrival(&self) -> TcpStream {
        self.listener.accept().await.unwrap().0
    }

    /// How many connections arrived before this call. It connects a probe,
    /// then accepts until the probe arrives. The kernel queues connections in
    /// the order they were made, so none is missed.
    pub async fn accepted(&self) -> usize {
        let probe = TcpStream::connect(self.addr).await.unwrap();
        let probe = probe.local_addr().unwrap();
        let mut before = 0;
        loop {
            let (_, peer) = self.listener.accept().await.unwrap();
            if peer == probe {
                return before;
            }
            before += 1;
        }
    }
}

/// Answers its first connection `503` with `Connection: close`. It stops
/// listening as soon as that connection is accepted, so every later connect
/// to its address is refused.
pub async fn one_503_then_gone() -> SocketAddr {
    let listener = bind().await;
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        drop(listener);
        read_head(&stream).await;
        write_all(
            &stream,
            b"HTTP/1.1 503 Service Unavailable\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
        )
        .await;
    });
    addr
}

/// Reads into `buf`. 0 means the stream ended or failed.
async fn read_some(stream: &TcpStream, buf: &mut [u8]) -> usize {
    loop {
        if stream.readable().await.is_err() {
            return 0;
        }
        match stream.try_read(buf) {
            Ok(read) => return read,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return 0,
        }
    }
}

/// Reads exactly `len` bytes, or fewer if the stream ends.
async fn read_exact(stream: &TcpStream, len: usize) -> Vec<u8> {
    let mut buf = vec![0; len];
    let mut filled = 0;
    while filled < len {
        let read = read_some(stream, &mut buf[filled..]).await;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buf.truncate(filled);
    buf
}

/// Reads until the end of a request head, or until the stream ends.
async fn read_head(stream: &TcpStream) {
    let mut seen = Vec::new();
    let mut buf = [0; 4096];
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = read_some(stream, &mut buf).await;
        if read == 0 {
            return;
        }
        seen.extend_from_slice(&buf[..read]);
    }
}

/// Writes every byte of `bytes`, or stops quietly if the peer is gone.
pub async fn write_all(stream: &TcpStream, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        if stream.writable().await.is_err() {
            return;
        }
        match stream.try_write(bytes) {
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return,
        }
    }
}

/// Reads the request head, writes `response`, then holds the connection
/// open without another byte.
fn answer(stream: TcpStream, response: &'static [u8]) -> BoxFuture {
    Box::pin(async move {
        read_head(&stream).await;
        write_all(&stream, response).await;
        pending::<()>().await;
    })
}

/// Accepts, then never reads or writes.
pub fn hold(stream: TcpStream) -> BoxFuture {
    Box::pin(async move {
        let _held = stream;
        pending::<()>().await;
    })
}

/// Reads the request head, then stays silent.
pub fn silent_after_head(stream: TcpStream) -> BoxFuture {
    Box::pin(async move {
        read_head(&stream).await;
        pending::<()>().await;
    })
}

/// Reads the request head, then closes the connection.
pub fn close_after_head(stream: TcpStream) -> BoxFuture {
    Box::pin(async move {
        read_head(&stream).await;
    })
}

/// Promises 10 body bytes, sends 3, then stays silent.
pub fn stalled_body(stream: TcpStream) -> BoxFuture {
    answer(stream, b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc")
}

/// Promises 2 MiB, sends 1 MiB, then holds the connection.
pub fn two_mib_promised_then_stall(stream: TcpStream) -> BoxFuture {
    Box::pin(async move {
        read_head(&stream).await;
        write_all(
            &stream,
            b"HTTP/1.1 200 OK\r\ncontent-length: 2097152\r\n\r\n",
        )
        .await;
        write_all(&stream, &vec![b'x'; MIB as usize]).await;
        pending::<()>().await;
    })
}

/// An `HTTP/1.0` answer with no length and 262144 bytes (256 KiB) in 64
/// writes of 4096, then a close.
pub fn http10_to_close(stream: TcpStream) -> BoxFuture {
    Box::pin(async move {
        read_head(&stream).await;
        write_all(&stream, b"HTTP/1.0 200 OK\r\n\r\n").await;
        for _ in 0..64 {
            write_all(&stream, &[b'x'; 4096]).await;
        }
    })
}

pub fn garbage_status(stream: TcpStream) -> BoxFuture {
    answer(stream, b"HELLO\r\n\r\n")
}

/// A chunked body whose second chunk has a size line that isn't hex.
pub fn malformed_chunk(stream: TcpStream) -> BoxFuture {
    answer(
        stream,
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\nzz\r\n",
    )
}

/// Two `set-cookie` headers and a value that isn't UTF-8.
pub fn raw_headers(stream: TcpStream) -> BoxFuture {
    answer(
        stream,
        b"HTTP/1.1 200 OK\r\nset-cookie: a=1\r\nset-cookie: b=2\r\nx-raw: \xff\xfe\r\ncontent-length: 0\r\n\r\n",
    )
}

/// A `200` whose head has exactly `count` header lines, `content-length: 0`
/// among them, then holds the connection open.
fn head_with_headers(stream: TcpStream, count: usize) -> BoxFuture {
    Box::pin(async move {
        read_head(&stream).await;
        let mut head = String::from("HTTP/1.1 200 OK\r\n");
        for n in 0..count - 1 {
            head.push_str(&format!("x-{n}: v\r\n"));
        }
        head.push_str("content-length: 0\r\n\r\n");
        write_all(&stream, head.as_bytes()).await;
        pending::<()>().await;
    })
}

/// The HTTP/1.1 head buffer `vush-http` gives hyper: 417,792 bytes, which
/// is 408 KiB.
pub const HEAD_BUFFER_BYTES: usize = 417_792;

/// A `200` head of exactly `len` bytes, counting its status line, its
/// headers and the empty line that ends it. Its `x-big` header is padded to
/// make up the length, and `content-length: 0` follows it.
pub fn head_of_len(len: usize) -> Vec<u8> {
    let start = b"HTTP/1.1 200 OK\r\nx-big: ";
    let end = b"\r\ncontent-length: 0\r\n\r\n";
    let mut head = start.to_vec();
    head.resize(len - end.len(), b'a');
    head.extend_from_slice(end);
    head
}

/// Writes a head of exactly `len` bytes, then holds the connection open.
fn head_of(stream: TcpStream, len: usize) -> BoxFuture {
    Box::pin(async move {
        read_head(&stream).await;
        write_all(&stream, &head_of_len(len)).await;
        pending::<()>().await;
    })
}

/// A head that fills the head buffer exactly.
pub fn head_of_the_buffer(stream: TcpStream) -> BoxFuture {
    head_of(stream, HEAD_BUFFER_BYTES)
}

/// A head of 1 MB, far over the head buffer.
pub fn head_of_1_mb(stream: TcpStream) -> BoxFuture {
    head_of(stream, 1_000_000)
}

/// A `200` with exactly 100 header lines, hyper's default limit.
pub fn hundred_headers(stream: TcpStream) -> BoxFuture {
    head_with_headers(stream, 100)
}

/// A `200` with 101 header lines, one past hyper's default limit.
pub fn hundred_and_one_headers(stream: TcpStream) -> BoxFuture {
    head_with_headers(stream, 101)
}

/// Sets a cookie, promises 10 body bytes, sends 3, then closes.
pub fn cookie_then_cut_body(stream: TcpStream) -> BoxFuture {
    Box::pin(async move {
        read_head(&stream).await;
        write_all(
            &stream,
            b"HTTP/1.1 200 OK\r\nset-cookie: late=1\r\ncontent-length: 10\r\n\r\nabc",
        )
        .await;
    })
}

/// A `set-cookie` value that isn't UTF-8.
pub fn raw_cookie(stream: TcpStream) -> BoxFuture {
    answer(
        stream,
        b"HTTP/1.1 200 OK\r\nset-cookie: raw=\xff\xfe\r\ncontent-length: 0\r\n\r\n",
    )
}

/// A head with `content-length: 100` and no body, as a `HEAD` gets.
pub fn head_with_length(stream: TcpStream) -> BoxFuture {
    answer(stream, b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n")
}

pub fn no_content(stream: TcpStream) -> BoxFuture {
    answer(stream, b"HTTP/1.1 204 No Content\r\n\r\n")
}

/// Speaks just enough HTTP/2 (prior knowledge) to answer stream 1 with a
/// head and one `DATA` frame, then reset the stream with `INTERNAL_ERROR`.
///
/// The frames are written in order on the socket. A hyper server can't
/// promise that: h2 drops a stream's queued frames when the stream is reset,
/// so its head could be lost and the reset land before it.
pub fn h2_reset_after_one_frame(stream: TcpStream) -> BoxFuture {
    Box::pin(async move {
        // The client's preface, then its frames up to the request's HEADERS.
        read_exact(&stream, 24).await;
        loop {
            let header = read_exact(&stream, 9).await;
            if header.len() < 9 {
                return;
            }
            let len = u32::from_be_bytes([0, header[0], header[1], header[2]]);
            read_exact(&stream, len as usize).await;
            if header[3] == 0x1 {
                break;
            }
        }
        #[rustfmt::skip]
        let frames: &[u8] = &[
            // SETTINGS, empty
            0, 0, 0, 0x4, 0x0, 0, 0, 0, 0,
            // SETTINGS with ACK, for the client's
            0, 0, 0, 0x4, 0x1, 0, 0, 0, 0,
            // HEADERS on stream 1 with END_HEADERS: `:status: 200` (HPACK index 8)
            0, 0, 1, 0x1, 0x4, 0, 0, 0, 1, 0x88,
            // DATA on stream 1: "first"
            0, 0, 5, 0x0, 0x0, 0, 0, 0, 1, b'f', b'i', b'r', b's', b't',
            // RST_STREAM on stream 1: INTERNAL_ERROR
            0, 0, 4, 0x3, 0x0, 0, 0, 0, 1, 0, 0, 0, 0x2,
        ];
        write_all(&stream, frames).await;
        pending::<()>().await;
    })
}
