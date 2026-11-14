//! Vush's own connector, which hyper-util calls for each new connection.
//!
//! It resolves the host, connects, and runs the TLS handshake for `https`,
//! timing each step. Each connection gets a [`ConnInfo`] with a process-wide
//! ID and its setup times, which hyper-util copies into every response.
//!
//! Its errors carry a kind and a fixed message, never a host, a port or a
//! library's text.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Instant;

use http::Uri;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use rustls::{AlertDescription, ClientConfig};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use vush_core::plan::HttpVersion;
use vush_core::protocol::{ConnectionId, ConnectionSetup, FailureKind, FailureStage};

use crate::stage::mark;

/// The next connection ID. Every connector in the process shares it, so IDs
/// never repeat across executors.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// What a connection tells the hops that use it.
pub(crate) struct ConnInfo {
    pub id: ConnectionId,
    setup: Mutex<Option<ConnectionSetup>>,
}

impl ConnInfo {
    /// The setup times, for the first hop that asks. Later calls get `None`.
    pub(crate) fn take_setup(&self) -> Option<ConnectionSetup> {
        self.setup
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// Why no connection was opened. It holds only a kind and a fixed message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectError {
    pub kind: FailureKind,
    pub message: &'static str,
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for ConnectError {}

const fn error(kind: FailureKind, message: &'static str) -> ConnectError {
    ConnectError { kind, message }
}

const UNSUPPORTED_SCHEME: ConnectError = error(
    FailureKind::InvalidRequest,
    "only http and https URLs are supported",
);
// `prepare` refuses these URLs first, so the connector should never see one.
// The texts are `prepare`'s.
const NO_HOST: ConnectError = error(FailureKind::InvalidRequest, "the URL has no host");
const UNPARSABLE: ConnectError = error(FailureKind::InvalidRequest, "the URL can't be parsed");
const UNRESOLVED: ConnectError = error(FailureKind::Dns, "the host name didn't resolve");
const UNREACHABLE: ConnectError = error(
    FailureKind::Connect,
    "the connection was refused or unreachable",
);
const HANDSHAKE_FAILED: ConnectError = error(FailureKind::Tls, "the TLS handshake failed");
const NO_HTTP2: ConnectError = error(FailureKind::Tls, "the server didn't agree to HTTP/2");

/// Looks up a host name's addresses for a port. A seam, so a test can fail a
/// lookup without asking a real resolver.
pub(crate) type Resolver = Arc<
    dyn Fn(String, u16) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>>
        + Send
        + Sync,
>;

/// The system's resolver.
fn lookup(
    host: String,
    port: u16,
) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>> {
    Box::pin(async move {
        Ok(tokio::net::lookup_host((host.as_str(), port))
            .await?
            .collect())
    })
}

/// The host part of a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Host {
    /// An IP literal, without the brackets of an IPv6 address.
    Ip(IpAddr),
    /// A name to resolve.
    Name(String),
}

/// The host and port to connect to. Only `http` and `https` are accepted, and
/// their default ports are 80 and 443.
pub(crate) fn host_and_port(uri: &Uri) -> Result<(Host, u16), ConnectError> {
    let default_port = match uri.scheme_str() {
        Some("http") => 80,
        Some("https") => 443,
        _ => return Err(UNSUPPORTED_SCHEME),
    };
    let authority = uri.authority().ok_or(NO_HOST)?;
    let host = authority.host();
    if host.is_empty() {
        return Err(NO_HOST);
    }
    // `Authority::port` reads a port that isn't a `u16` as no port at all, so
    // the text after the host is read here instead.
    let after_userinfo = authority.as_str().rsplit('@').next().unwrap_or_default();
    let port = match after_userinfo
        .strip_prefix(host)
        .and_then(|rest| rest.strip_prefix(':'))
    {
        None | Some("") => default_port,
        Some(digits) => digits.parse().map_err(|_| UNPARSABLE)?,
    };
    let host = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(literal) => Host::Ip(IpAddr::V6(
            literal.parse::<Ipv6Addr>().map_err(|_| UNPARSABLE)?,
        )),
        None => match host.parse::<IpAddr>() {
            Ok(ip) => Host::Ip(ip),
            Err(_) => Host::Name(host.to_owned()),
        },
    };
    Ok((host, port))
}

/// Opens connections for hyper-util's client. Clones share their settings.
#[derive(Clone)]
pub(crate) struct Connector {
    tls: TlsConnector,
    version: HttpVersion,
    resolver: Resolver,
}

impl Connector {
    /// A connector that uses `tls` for `https` and the system's resolver.
    /// `version` must be the one `tls` was built for.
    pub(crate) fn new(tls: Arc<ClientConfig>, version: HttpVersion) -> Self {
        Self {
            tls: TlsConnector::from(tls),
            version,
            resolver: Arc::new(lookup),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_resolver(
        tls: Arc<ClientConfig>,
        version: HttpVersion,
        resolver: Resolver,
    ) -> Self {
        Self {
            resolver,
            ..Self::new(tls, version)
        }
    }

    async fn connect(self, uri: Uri) -> Result<Conn, ConnectError> {
        mark(FailureStage::Connect);
        let (host, port) = host_and_port(&uri)?;

        let (addrs, dns) = match &host {
            Host::Ip(ip) => (vec![SocketAddr::new(*ip, port)], None),
            Host::Name(name) => {
                let start = Instant::now();
                let addrs = (self.resolver)(name.clone(), port)
                    .await
                    .map_err(|_| UNRESOLVED)?;
                if addrs.is_empty() {
                    return Err(UNRESOLVED);
                }
                (addrs, Some(start.elapsed()))
            }
        };

        let start = Instant::now();
        let tcp = connect_in_order(&addrs).await?;
        let connect = start.elapsed();
        // Only latency depends on it, and hyper-util's own connector also
        // carries on when it can't be set.
        let _ = tcp.set_nodelay(true);

        let (stream, tls, h2) = if uri.scheme_str() == Some("https") {
            let name = match host {
                Host::Ip(ip) => ServerName::from(ip),
                Host::Name(name) => ServerName::try_from(name).map_err(|_| HANDSHAKE_FAILED)?,
            };
            let start = Instant::now();
            let stream = self
                .tls
                .connect(name, tcp)
                .await
                .map_err(|error| self.handshake_error(&error))?;
            let tls = start.elapsed();
            let h2 = stream.get_ref().1.alpn_protocol() == Some(b"h2".as_slice());
            // rustls accepts a server that chose no protocol, and HTTP/2 would
            // then be spoken blind.
            if self.version == HttpVersion::Http2 && !h2 {
                return Err(NO_HTTP2);
            }
            (Stream::Tls(Box::new(TokioIo::new(stream))), Some(tls), h2)
        } else {
            (Stream::Plain(TokioIo::new(tcp)), None, false)
        };

        let info = ConnInfo {
            id: ConnectionId(NEXT_ID.fetch_add(1, Ordering::Relaxed)),
            setup: Mutex::new(Some(ConnectionSetup { dns, connect, tls })),
        };
        Ok(Conn {
            stream,
            info: Arc::new(info),
            h2,
        })
    }

    /// A server that shares none of the offered protocols refuses the
    /// handshake with an alert. Under `http2` the only offer is `h2`, so the
    /// alert means the server didn't agree to HTTP/2.
    fn handshake_error(&self, error: &io::Error) -> ConnectError {
        let refused_alpn = matches!(
            error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<rustls::Error>()),
            Some(rustls::Error::AlertReceived(
                AlertDescription::NoApplicationProtocol
            ))
        );
        if self.version == HttpVersion::Http2 && refused_alpn {
            NO_HTTP2
        } else {
            HANDSHAKE_FAILED
        }
    }
}

/// Tries each address in order until one accepts. No racing.
async fn connect_in_order(addrs: &[SocketAddr]) -> Result<TcpStream, ConnectError> {
    for addr in addrs {
        if let Ok(tcp) = TcpStream::connect(*addr).await {
            return Ok(tcp);
        }
    }
    Err(UNREACHABLE)
}

impl tower_service::Service<Uri> for Connector {
    type Response = Conn;
    type Error = ConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<Conn, ConnectError>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), ConnectError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        Box::pin(self.clone().connect(uri))
    }
}

/// A connection the connector opened.
pub(crate) struct Conn {
    stream: Stream,
    info: Arc<ConnInfo>,
    h2: bool,
}

enum Stream {
    Plain(TokioIo<TcpStream>),
    // Boxed, because a TLS session is far larger than a socket.
    Tls(Box<TokioIo<TlsStream<TcpStream>>>),
}

impl Connection for Conn {
    fn connected(&self) -> Connected {
        let connected = Connected::new().extra(self.info.clone());
        if self.h2 {
            connected.negotiated_h2()
        } else {
            connected
        }
    }
}

impl Read for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.get_mut().stream {
            Stream::Plain(io) => Pin::new(io).poll_read(cx, buf),
            Stream::Tls(io) => Pin::new(&mut **io).poll_read(cx, buf),
        }
    }
}

impl Write for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.get_mut().stream {
            Stream::Plain(io) => Pin::new(io).poll_write(cx, buf),
            Stream::Tls(io) => Pin::new(&mut **io).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().stream {
            Stream::Plain(io) => Pin::new(io).poll_flush(cx),
            Stream::Tls(io) => Pin::new(&mut **io).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().stream {
            Stream::Plain(io) => Pin::new(io).poll_shutdown(cx),
            Stream::Tls(io) => Pin::new(&mut **io).poll_shutdown(cx),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match &self.stream {
            Stream::Plain(io) => io.is_write_vectored(),
            Stream::Tls(io) => io.is_write_vectored(),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match &mut self.get_mut().stream {
            Stream::Plain(io) => Pin::new(io).poll_write_vectored(cx, bufs),
            Stream::Tls(io) => Pin::new(&mut **io).poll_write_vectored(cx, bufs),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::atomic::AtomicUsize;

    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair,
        KeyUsagePurpose, SanType,
    };
    use rustls::RootCertStore;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tower_service::Service;

    use super::*;
    use crate::stage::{MARKER, StageMarker};
    use crate::tls::client_config;

    type Ca = CertifiedIssuer<'static, KeyPair>;
    type Leaf = (CertificateDer<'static>, PrivateKeyDer<'static>);
    /// A version mode, the server's ALPN protocols, and whether the
    /// connection speaks HTTP/2.
    type AlpnCase = (
        HttpVersion,
        &'static [&'static [u8]],
        Result<bool, ConnectError>,
    );

    const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    const UNSUPPORTED: ConnectError = ConnectError {
        kind: FailureKind::InvalidRequest,
        message: "only http and https URLs are supported",
    };
    const UNRESOLVED: ConnectError = ConnectError {
        kind: FailureKind::Dns,
        message: "the host name didn't resolve",
    };
    const REFUSED: ConnectError = ConnectError {
        kind: FailureKind::Connect,
        message: "the connection was refused or unreachable",
    };
    const HANDSHAKE: ConnectError = ConnectError {
        kind: FailureKind::Tls,
        message: "the TLS handshake failed",
    };
    const NO_HTTP2: ConnectError = ConnectError {
        kind: FailureKind::Tls,
        message: "the server didn't agree to HTTP/2",
    };

    fn ca() -> (RootCertStore, Ca) {
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "vush test ca");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        (roots, ca)
    }

    fn leaf_params(sans: &[SanType]) -> CertificateParams {
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "vush test leaf");
        params.subject_alt_names = sans.to_vec();
        params
    }

    fn private_key(key: &KeyPair) -> PrivateKeyDer<'static> {
        PrivatePkcs8KeyDer::from(key.serialize_der()).into()
    }

    fn leaf(ca: &Ca, sans: &[SanType]) -> Leaf {
        let key = KeyPair::generate().unwrap();
        let cert = leaf_params(sans).signed_by(&key, ca).unwrap();
        (cert.der().clone(), private_key(&key))
    }

    fn self_signed_leaf(sans: &[SanType]) -> Leaf {
        let key = KeyPair::generate().unwrap();
        let cert = leaf_params(sans).self_signed(&key).unwrap();
        (cert.der().clone(), private_key(&key))
    }

    fn localhost_san() -> [SanType; 1] {
        [SanType::IpAddress(LOCALHOST)]
    }

    /// Accepts in a loop and runs each handshake. Each stream, or what is left
    /// of a failed handshake, is held open until the test ends.
    async fn tls_server((cert, key): Leaf, alpn: &[&[u8]]) -> SocketAddr {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        config.alpn_protocols = alpn.iter().map(|name| name.to_vec()).collect();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind((LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _held = acceptor.accept(tcp).into_fallible().await;
                    std::future::pending::<()>().await;
                });
            }
        });
        addr
    }

    /// Accepts in a loop and holds each stream until the test ends.
    async fn tcp_server() -> SocketAddr {
        let listener = TcpListener::bind((LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        addr
    }

    /// An address on 127.0.0.1 where nothing listens any more.
    async fn refused_addr() -> SocketAddr {
        let listener = TcpListener::bind((LOCALHOST, 0)).await.unwrap();
        listener.local_addr().unwrap()
    }

    fn connector(roots: RootCertStore, version: HttpVersion) -> Connector {
        Connector::new(client_config(roots, version), version)
    }

    fn plain() -> Connector {
        connector(RootCertStore::empty(), HttpVersion::Auto)
    }

    fn resolving(resolver: Resolver) -> Connector {
        let tls = client_config(RootCertStore::empty(), HttpVersion::Auto);
        Connector::with_resolver(tls, HttpVersion::Auto, resolver)
    }

    fn failing_resolver() -> Resolver {
        Arc::new(|_, _| Box::pin(async { Err(io::Error::other("no such host")) }))
    }

    async fn connect(connector: &mut Connector, url: &str) -> Result<Conn, ConnectError> {
        connector.call(url.parse().unwrap()).await
    }

    async fn connect_err(connector: &mut Connector, url: &str) -> ConnectError {
        connect(connector, url)
            .await
            .err()
            .expect("the connect should fail")
    }

    /// The connection's info, read the way hyper-util hands it to a response.
    fn info(conn: &Conn) -> Arc<ConnInfo> {
        let mut extensions = http::Extensions::new();
        conn.connected().get_extras(&mut extensions);
        extensions
            .get::<Arc<ConnInfo>>()
            .cloned()
            .expect("every connection carries its info")
    }

    fn uri(text: &str) -> http::Uri {
        text.parse().unwrap()
    }

    #[tokio::test]
    async fn plain_connections_get_fresh_ids_and_setup_times() {
        let url = format!("http://{}/", tcp_server().await);
        let mut connector = plain();
        let first = connect(&mut connector, &url).await.unwrap();
        let second = connect(&mut connector, &url).await.unwrap();
        assert!(!first.connected().is_negotiated_h2());
        let (first, second) = (info(&first), info(&second));
        assert!(second.id.0 > first.id.0);
        let setup = first.take_setup().expect("a new connection has its setup");
        assert_eq!(setup.dns, None);
        assert_eq!(setup.tls, None);
        assert_eq!(first.take_setup(), None);
        assert!(second.take_setup().is_some());
    }

    #[tokio::test]
    async fn ids_are_unique_across_connectors() {
        let url = format!("http://{}/", tcp_server().await);
        let a = info(&connect(&mut plain(), &url).await.unwrap()).id;
        let b = info(&connect(&mut plain(), &url).await.unwrap()).id;
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn trusted_tls_negotiates_alpn() {
        let (roots, ca) = ca();
        let addr = tls_server(leaf(&ca, &localhost_san()), &[b"h2", b"http/1.1"]).await;
        let mut connector = connector(roots, HttpVersion::Auto);
        let conn = connect(&mut connector, &format!("https://{addr}/"))
            .await
            .unwrap();
        assert!(conn.connected().is_negotiated_h2());
        let setup = info(&conn).take_setup().unwrap();
        assert!(setup.tls.is_some());
        assert_eq!(setup.dns, None);
    }

    #[tokio::test]
    async fn alpn_follows_the_version_mode() {
        let cases: [AlpnCase; 5] = [
            (HttpVersion::Http1, &[b"h2", b"http/1.1"], Ok(false)),
            (HttpVersion::Auto, &[b"http/1.1"], Ok(false)),
            (HttpVersion::Http2, &[b"h2", b"http/1.1"], Ok(true)),
            (HttpVersion::Http2, &[b"http/1.1"], Err(NO_HTTP2)),
            (HttpVersion::Http2, &[], Err(NO_HTTP2)),
        ];
        for (version, offer, expected) in cases {
            let (roots, ca) = ca();
            let addr = tls_server(leaf(&ca, &localhost_san()), offer).await;
            let got = connect(&mut connector(roots, version), &format!("https://{addr}/"))
                .await
                .map(|conn| conn.connected().is_negotiated_h2());
            assert_eq!(
                got, expected,
                "{version:?} against a server offering {offer:?}"
            );
        }
    }

    #[tokio::test]
    async fn untrusted_and_wrong_name_certificates_fail_as_tls() {
        let (roots, ca) = ca();
        let other_name = [SanType::DnsName("other.test".try_into().unwrap())];
        let untrusted = tls_server(self_signed_leaf(&localhost_san()), &[b"http/1.1"]).await;
        let wrong_name = tls_server(leaf(&ca, &other_name), &[b"http/1.1"]).await;
        for addr in [untrusted, wrong_name] {
            let mut connector = connector(roots.clone(), HttpVersion::Auto);
            let error = connect_err(&mut connector, &format!("https://{addr}/")).await;
            assert_eq!(error, HANDSHAKE);
        }
    }

    #[tokio::test]
    async fn a_refused_port_fails_as_connect() {
        let addr = refused_addr().await;
        let error = connect_err(&mut plain(), &format!("http://{addr}/")).await;
        assert_eq!(error, REFUSED);
    }

    #[tokio::test]
    async fn a_failing_resolver_fails_as_dns() {
        let empty: Resolver = Arc::new(|_, _| Box::pin(async { Ok(Vec::new()) }));
        for resolver in [failing_resolver(), empty] {
            let error = connect_err(&mut resolving(resolver), "http://unresolvable.test/").await;
            assert_eq!(error, UNRESOLVED);
        }
    }

    #[tokio::test]
    async fn a_resolved_name_is_timed_and_tried_address_by_address() {
        let addrs = vec![refused_addr().await, tcp_server().await];
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let resolver: Resolver = Arc::new(move |host, port| {
            seen.lock().unwrap().push((host, port));
            let addrs = addrs.clone();
            Box::pin(async move { Ok(addrs) })
        });
        let conn = connect(&mut resolving(resolver), "http://api.test/")
            .await
            .unwrap();
        assert_eq!(*asked.lock().unwrap(), [("api.test".to_owned(), 80)]);
        let setup = info(&conn).take_setup().unwrap();
        assert!(setup.dns.is_some());
        assert_eq!(setup.tls, None);
    }

    #[tokio::test]
    async fn unsupported_schemes_are_refused_before_resolving() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let counting: Resolver = Arc::new(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Vec::new()) })
        });
        let mut connector = resolving(counting);
        for url in ["ftp://127.0.0.1:1/", "ftp://unresolvable.test/"] {
            assert_eq!(connect_err(&mut connector, url).await, UNSUPPORTED);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ipv6_literals_lose_their_brackets() {
        let v6 = Host::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert_eq!(
            host_and_port(&uri("http://[::1]:8080/")),
            Ok((v6.clone(), 8080))
        );
        assert_eq!(host_and_port(&uri("https://[::1]/")), Ok((v6, 443)));
        assert_eq!(
            host_and_port(&uri("http://127.0.0.1/")),
            Ok((Host::Ip(LOCALHOST), 80))
        );
        assert_eq!(
            host_and_port(&uri("https://api.test:8443/")),
            Ok((Host::Name("api.test".to_owned()), 8443))
        );
    }

    #[test]
    fn unusable_ports_and_hosts_are_refused() {
        // The http crate parses all three.
        for url in [
            "http://api.test:99999/",
            "http://api.test:8o/",
            "http://[::g]/",
        ] {
            assert_eq!(host_and_port(&uri(url)), Err(UNPARSABLE), "{url}");
        }
        assert_eq!(host_and_port(&uri("http://:80/")), Err(NO_HOST));
        assert_eq!(
            host_and_port(&uri("http://api.test:/")),
            Ok((Host::Name("api.test".to_owned()), 80))
        );
    }

    #[tokio::test]
    async fn the_connector_marks_the_connect_stage() {
        let url = format!("http://{}/", tcp_server().await);
        let marker = StageMarker::new();
        assert_eq!(marker.get(), FailureStage::Prepare);
        let mut connector = plain();
        MARKER
            .scope(marker.clone(), connect(&mut connector, &url))
            .await
            .unwrap();
        assert_eq!(marker.get(), FailureStage::Connect);
    }

    #[tokio::test]
    async fn connect_errors_hold_no_host_or_port() {
        let (roots, ca) = ca();
        let refused = refused_addr().await;
        let untrusted = tls_server(self_signed_leaf(&localhost_san()), &[]).await;
        let other_name = [SanType::DnsName("other.test".try_into().unwrap())];
        let wrong_name = tls_server(leaf(&ca, &other_name), &[]).await;
        let no_h2 = tls_server(leaf(&ca, &localhost_san()), &[b"http/1.1"]).await;

        let mut errors = vec![
            connect_err(&mut plain(), &format!("ftp://{refused}/")).await,
            connect_err(
                &mut resolving(failing_resolver()),
                "http://unresolvable.test/",
            )
            .await,
            connect_err(&mut plain(), &format!("http://{refused}/")).await,
        ];
        for addr in [untrusted, wrong_name] {
            let mut connector = connector(roots.clone(), HttpVersion::Auto);
            errors.push(connect_err(&mut connector, &format!("https://{addr}/")).await);
        }
        let mut http2 = connector(roots, HttpVersion::Http2);
        errors.push(connect_err(&mut http2, &format!("https://{no_h2}/")).await);

        assert_eq!(
            errors,
            [
                UNSUPPORTED,
                UNRESOLVED,
                REFUSED,
                HANDSHAKE,
                HANDSHAKE,
                NO_HTTP2
            ]
        );
        let ports = [refused, untrusted, wrong_name, no_h2].map(|addr| addr.port().to_string());
        for error in errors {
            assert_eq!(error.to_string(), error.message);
            let shown = format!("{error} {error:?}");
            for secret in ["127.0.0.1", "unresolvable"] {
                assert!(!shown.contains(secret), "{shown}");
            }
            for port in &ports {
                assert!(!shown.contains(port.as_str()), "{shown}");
            }
        }
    }
}
