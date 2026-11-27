//! HTTP/2 on h2, through the stream gate.
//!
//! - **The connection** is h2's client handshake over the connector's stream,
//!   wrapped in `TokioIo`, with the settings hyper's client uses except one:
//!   until the server's first `SETTINGS` arrive, a new connection assumes one
//!   stream, where hyper assumes 100. Its task runs on the Tokio runtime.
//!   After each poll of h2's connection, the task gives the gate the server's
//!   current limit, then polls a sender of its own, with a waker that does
//!   nothing, to learn of a `GOAWAY` or a connection error. Either retires the
//!   connection: it takes no more streams and its gate closes, while its open
//!   streams go on. When the last handle the pool and its hops hold is gone,
//!   the task drops its sender, so h2 closes the connection once its streams
//!   end.
//! - **A request** is handed to h2 as hyper's HTTP/2 client handed it: the URI
//!   stays absolute, for h2 to turn into the pseudo-headers, the headers go as
//!   `prepare` made them, `content-length` follows hyper's rule, and an empty
//!   body ends the stream with the head. Each request goes on a fresh clone of
//!   the connection's sender, so h2's rule of one waiting stream per sender
//!   never turns one down.
//! - **The body** goes in chunks of the capacity h2 assigns, inside the hop:
//!   alongside the wait for the head, then inside the response body. A hop
//!   that ends stops its upload.
//! - **The response body** gives each data frame's capacity back to h2 as it
//!   is read. A reset with `NO_ERROR` ends it, as hyper ends it: the server
//!   answered before reading the whole request (RFC 9113 §8.1).
//!
//! Each stream's handles drop before its slot, so the slot of a stream that
//! ended normally never frees while h2 still counts that stream. h2 counts
//! a stream that ends with a reset, because its hop ended early or the
//! server answered before the upload was done, until the connection's task
//! writes the reset, so the request that takes its slot waits for that
//! write.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Waker, ready};

use bytes::Bytes;
use h2::client::{Builder, Connection, ResponseFuture, SendRequest};
use h2::{Reason, RecvStream, SendStream};
use http::header::CONTENT_LENGTH;
use http::{HeaderValue, Method, Request, Response};
use hyper::body::Frame;
use hyper_util::rt::TokioIo;

use super::gate::{Gate, Slot};
use super::handoff::{Giver, Taker, handoff};
use crate::connector::{Conn, ConnInfo};
use crate::executor::H2_HEADER_LIST_BYTES;

/// The streams a new connection assumes until the server's first `SETTINGS`.
const INITIAL_MAX_SEND_STREAMS: usize = 1;
/// The receive window of each stream: 2 MiB, as hyper's.
const STREAM_WINDOW: u32 = 2_097_152;
/// The receive window of the connection: 5 MiB, as hyper's.
const CONNECTION_WINDOW: u32 = 5_242_880;
/// The largest frame the server may send: 16 KiB, as hyper's.
const MAX_FRAME_SIZE: u32 = 16_384;
/// The most body bytes h2 buffers for one stream: 1 MiB, as hyper's. It is
/// also the most capacity a body asks for at a time.
const MAX_SEND_BUFFER: usize = 1_048_576;

/// The connection's h2 client, over the connector's stream.
pub(super) type H2Connection = Connection<TokioIo<Conn>, Bytes>;

/// Runs h2's client handshake on `conn`. It writes the preface to the
/// stream itself, and the client's `SETTINGS` into the connection's buffer,
/// which the connection's task sends.
pub(super) async fn handshake(conn: Conn) -> Result<(SendRequest<Bytes>, H2Connection), h2::Error> {
    let mut builder = Builder::new();
    // hyper also sets `max_local_error_reset_streams` to 1024, which is h2's
    // default. Push, an adaptive window and pings stay off.
    builder
        .initial_max_send_streams(INITIAL_MAX_SEND_STREAMS)
        .initial_window_size(STREAM_WINDOW)
        .initial_connection_window_size(CONNECTION_WINDOW)
        .max_frame_size(MAX_FRAME_SIZE)
        .max_send_buffer_size(MAX_SEND_BUFFER)
        .max_header_list_size(H2_HEADER_LIST_BYTES)
        .enable_push(false);
    builder.handshake(TokioIo::new(conn)).await
}

/// The head h2 is handed for `request`. It sets `content-length` when the
/// request has none and either the body isn't empty or the method defines
/// a body, which `GET`, `HEAD`, `DELETE` and `CONNECT` don't: hyper's rule.
pub(super) fn h2_request(request: &Request<Bytes>) -> Request<()> {
    let mut head = Request::new(());
    *head.method_mut() = request.method().clone();
    *head.uri_mut() = request.uri().clone();
    *head.headers_mut() = request.headers().clone();
    let len = request.body().len();
    let defines_a_body = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::DELETE | Method::CONNECT
    );
    if (len != 0 || defines_a_body) && !head.headers().contains_key(CONTENT_LENGTH) {
        // `prepare` caps the headers far below the map's limit, so the
        // insert can't fail.
        let _ = head
            .headers_mut()
            .try_insert(CONTENT_LENGTH, HeaderValue::from(len));
    }
    head
}

/// What an HTTP/2 connection's task shares with the pool and its hops.
pub(super) struct Link {
    /// The gate every stream on the connection goes through.
    pub(super) gate: Arc<Gate>,
    /// Set once the connection takes no more streams.
    draining: AtomicBool,
    /// Set once the server sent `GOAWAY`.
    goaway: AtomicBool,
    /// Set once h2 took a request on the connection.
    opened: AtomicBool,
}

impl Link {
    /// Marks the connection as taking no more streams, because the server
    /// sent `GOAWAY` when `goaway`.
    pub(super) fn drain(&self, goaway: bool) {
        if goaway {
            self.goaway.store(true, Ordering::SeqCst);
        }
        self.draining.store(true, Ordering::SeqCst);
    }

    /// Whether the connection takes no more streams.
    pub(super) fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// Whether the connection turned requests down because of a `GOAWAY`,
    /// after it had opened at least one stream (see `after_unsent`).
    pub(super) fn drained(&self) -> bool {
        self.goaway.load(Ordering::SeqCst) && self.opened.load(Ordering::SeqCst)
    }
}

/// An HTTP/2 connection, as the pool holds it and hands it to hops. It holds
/// a host through its sender, so it has no `Debug`.
pub(super) struct H2Conn {
    sender: SendRequest<Bytes>,
    pub(super) link: Arc<Link>,
    pub(super) info: Arc<ConnInfo>,
    /// Dropped with the last handle, which tells the task to let h2 close the
    /// connection once its streams end.
    _alive: Giver<()>,
}

/// A request h2 took: its response, and the stream its body goes on.
pub(super) struct Opened {
    response: ResponseFuture,
    stream: SendStream<Bytes>,
}

/// Why h2 turned a request down: the connection can take no more streams.
/// `prepare` refuses every header h2 would turn down, and every URL is
/// absolute, so h2 turns a request down only for its connection: after a
/// `GOAWAY` or a connection error, or once its stream IDs are used up.
pub(super) struct Refused {
    /// Whether the server's `GOAWAY` was the cause.
    pub(super) goaway: bool,
}

impl H2Conn {
    /// Hands h2 `request` on a fresh clone of the connection's sender. An
    /// empty body ends the stream with the head.
    pub(super) fn open(&self, request: &Request<Bytes>) -> Result<Opened, Refused> {
        let end_of_stream = request.body().is_empty();
        match self
            .sender
            .clone()
            .send_request(h2_request(request), end_of_stream)
        {
            Ok((response, stream)) => {
                self.link.opened.store(true, Ordering::SeqCst);
                Ok(Opened { response, stream })
            }
            Err(error) => Err(Refused {
                goaway: error.is_go_away() && error.is_remote(),
            }),
        }
    }
}

/// Spawns the task that runs `connection`, and gives its handle. The task
/// calls `retire` when h2 turns its probe down, with whether a `GOAWAY` was
/// the cause, and when the connection ends.
pub(super) fn start<R>(
    sender: SendRequest<Bytes>,
    connection: H2Connection,
    info: Arc<ConnInfo>,
    retire: R,
) -> Arc<H2Conn>
where
    R: FnMut(&Arc<Link>, bool) + Send + Unpin + 'static,
{
    let link = Arc::new(Link {
        gate: Gate::new(connection.max_concurrent_send_streams()),
        draining: AtomicBool::new(false),
        goaway: AtomicBool::new(false),
        opened: AtomicBool::new(false),
    });
    let (alive, handles_gone) = handoff();
    tokio::spawn(Task {
        connection,
        probe: Some(sender.clone()),
        handles_gone,
        link: link.clone(),
        retire,
    });
    Arc::new(H2Conn {
        sender,
        link,
        info,
        _alive: alive,
    })
}

/// An HTTP/2 connection's task.
struct Task<R> {
    connection: H2Connection,
    /// A sender of the task's own, polled after each poll of the connection.
    /// Dropped once the connection is retired, or every handle is gone.
    probe: Option<SendRequest<Bytes>>,
    /// Ready once every [`H2Conn`] handle has dropped.
    handles_gone: Taker<()>,
    link: Arc<Link>,
    retire: R,
}

impl<R> Future for Task<R>
where
    R: FnMut(&Arc<Link>, bool) + Unpin,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        loop {
            let ended = Pin::new(&mut this.connection).poll(cx).is_ready();
            // The server's `SETTINGS`, read in that poll, may have changed
            // its limit.
            this.link
                .gate
                .set_limit(this.connection.max_concurrent_send_streams());
            // h2 turns the probe down once the server sent `GOAWAY` or the
            // connection failed. The probe has no stream waiting, so it is
            // never `Pending`, and h2 keeps no waker of it.
            if let Some(probe) = &mut this.probe
                && let Poll::Ready(Err(error)) =
                    probe.poll_ready(&mut Context::from_waker(Waker::noop()))
            {
                (this.retire)(&this.link, error.is_go_away() && error.is_remote());
                this.probe = None;
            }
            if ended {
                (this.retire)(&this.link, false);
                return Poll::Ready(());
            }
            if this.probe.is_some() && Pin::new(&mut this.handles_gone).poll(cx).is_ready() {
                // Polled again at once, so h2 sees its last sender gone.
                this.probe = None;
                continue;
            }
            return Poll::Pending;
        }
    }
}

/// A hop's wait for its response head, with its body going out meanwhile.
/// Its fields drop in order, h2's handles before the slot.
pub(super) struct Exchange {
    response: ResponseFuture,
    upload: Option<Upload>,
    slot: Option<Slot>,
}

impl Exchange {
    /// The exchange of a request h2 took on the stream `slot` holds, with
    /// `body` to send. `sent` counts the body bytes handed to h2.
    pub(super) fn new(opened: Opened, body: Bytes, sent: Arc<AtomicU64>, slot: Slot) -> Self {
        let Opened { response, stream } = opened;
        // An empty body ended the stream with the head.
        let upload = if body.is_empty() {
            None
        } else {
            Some(Upload {
                stream,
                rest: body,
                sent,
            })
        };
        Self {
            response,
            upload,
            slot: Some(slot),
        }
    }
}

impl Future for Exchange {
    type Output = Result<Response<H2Body>, h2::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(upload) = &mut this.upload
            && upload.poll(cx).is_ready()
        {
            this.upload = None;
        }
        let head = ready!(Pin::new(&mut this.response).poll(cx))?;
        let slot = this
            .slot
            .take()
            .expect("an exchange is polled until its head");
        let upload = this.upload.take();
        Poll::Ready(Ok(head.map(|recv| H2Body {
            stream: Some(Streaming {
                recv,
                upload,
                data_done: false,
                _slot: slot,
            }),
        })))
    }
}

/// The rest of a request body, and the stream it goes on.
struct Upload {
    stream: SendStream<Bytes>,
    rest: Bytes,
    sent: Arc<AtomicU64>,
}

impl Upload {
    /// Hands h2 as much of the body as it has capacity for, in chunks of the
    /// capacity it assigns. The last chunk ends the stream. It is ready once
    /// the whole body is handed over, or when the stream can take no more
    /// because it was reset or closed.
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        while !self.rest.is_empty() {
            let capacity = self.stream.capacity();
            if capacity == 0 {
                self.stream
                    .reserve_capacity(self.rest.len().min(MAX_SEND_BUFFER));
                match ready!(self.stream.poll_capacity(cx)) {
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return Poll::Ready(()),
                }
            }
            let chunk = self.rest.split_to(capacity.min(self.rest.len()));
            let len = chunk.len() as u64;
            if self.stream.send_data(chunk, self.rest.is_empty()).is_err() {
                return Poll::Ready(());
            }
            self.sent.fetch_add(len, Ordering::Relaxed);
        }
        Poll::Ready(())
    }
}

/// An HTTP/2 response body. At its end, or when it fails, the stream's h2
/// handles drop, and then its slot.
pub(crate) struct H2Body {
    stream: Option<Streaming>,
}

/// A stream whose body is being read. Its fields drop in order, h2's handles
/// before the slot.
struct Streaming {
    recv: RecvStream,
    upload: Option<Upload>,
    data_done: bool,
    _slot: Slot,
}

impl H2Body {
    /// The body's next frame. Data and trailers come as h2 gives them, and
    /// the stream is let go after the last.
    pub(super) fn poll_frame(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, h2::Error>>> {
        let Some(streaming) = &mut self.stream else {
            return Poll::Ready(None);
        };
        let polled = ready!(streaming.poll_frame(cx));
        if !matches!(&polled, Some(Ok(frame)) if frame.is_data()) {
            self.stream = None;
        }
        Poll::Ready(polled)
    }
}

impl Streaming {
    fn poll_frame(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, h2::Error>>> {
        if let Some(upload) = &mut self.upload
            && upload.poll(cx).is_ready()
        {
            self.upload = None;
        }
        if !self.data_done {
            match ready!(self.recv.poll_data(cx)) {
                Some(Ok(data)) => {
                    // As hyper does, the window opens again as data is read.
                    let _ = self.recv.flow_control().release_capacity(data.len());
                    return Poll::Ready(Some(Ok(Frame::data(data))));
                }
                Some(Err(error)) => return Poll::Ready(failed(error)),
                None => self.data_done = true,
            }
        }
        Poll::Ready(match ready!(self.recv.poll_trailers(cx)) {
            Ok(trailers) => trailers.map(|trailers| Ok(Frame::trailers(trailers))),
            Err(error) => failed(error),
        })
    }
}

/// How a body ends on `error`: a `NO_ERROR` reset ends it, as hyper ends it,
/// and any other error fails it.
fn failed(error: h2::Error) -> Option<Result<Frame<Bytes>, h2::Error>> {
    (error.reason() != Some(Reason::NO_ERROR)).then_some(Err(error))
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use http::header::CONTENT_LENGTH;
    use http::{Request, Response};
    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tokio::net::TcpListener;
    use vush_core::plan::HttpVersion;

    use super::*;
    use crate::client::tests::{hop, pool};
    use crate::executor::IDLE_TIMEOUT;

    /// The `content-length` h2 is handed for `method` with `body`.
    fn content_length(method: &str, body: &'static [u8]) -> Option<String> {
        let request = Request::builder()
            .method(method)
            .uri("https://a.test/p")
            .body(Bytes::from_static(body))
            .unwrap();
        h2_request(&request)
            .headers()
            .get(CONTENT_LENGTH)
            .map(|length| length.to_str().unwrap().to_owned())
    }

    #[test]
    fn content_length_follows_hypers_rule() {
        assert_eq!(content_length("GET", b""), None);
        assert_eq!(content_length("POST", b""), Some("0".to_owned()));
        assert_eq!(content_length("POST", b"{\"a\":1}"), Some("7".to_owned()));
        assert_eq!(content_length("DELETE", b""), None);
        assert_eq!(content_length("DELETE", b"abc"), Some("3".to_owned()));
    }

    /// An HTTP/2 server with prior knowledge on 127.0.0.1 that answers every
    /// request `200`.
    async fn spawn_h2c() -> SocketAddr {
        let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let service = service_fn(|_| async {
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                    });
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(tcp), service)
                        .await;
                });
            }
        });
        addr
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sweep_closes_an_http2_connection_idle_for_90_s() {
        let (a, b) = (spawn_h2c().await, spawn_h2c().await);
        let pool = pool(HttpVersion::Http2);
        for addr in [a, b] {
            assert!(hop(&pool, addr).await.setup.is_some());
        }
        let t0 = Instant::now();
        assert_eq!(pool.origins(), 2);

        pool.sweep(t0 + Duration::from_secs(89));
        assert_eq!(pool.origins(), 2, "both connections were kept");

        pool.sweep(t0 + IDLE_TIMEOUT + Duration::from_millis(1));
        assert_eq!(pool.origins(), 0);
        for addr in [a, b] {
            assert!(hop(&pool, addr).await.setup.is_some(), "a new connection");
        }
    }
}
