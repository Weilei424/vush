//! HTTP/1.1 on hyper's connection API.
//!
//! A connection is hyper's HTTP/1.1 handshake over the connector's stream,
//! or any other I/O, built with `max_buf_size(HEAD_BUFFER_BYTES)` and
//! otherwise hyper's defaults, as hyper-util's client built it. Its task runs
//! on the Tokio runtime and ends when the connection closes, which hyper does
//! once its sender is dropped.
//!
//! A request goes out as hyper-util sent it: with `Host` added when missing,
//! in origin form, and with hyper's own framing from the body's exact size.
//! hyper checks whether the hop was dropped just before it writes the head,
//! so a hop dropped first sends nothing.

use http::header::HOST;
use http::{HeaderValue, Request, Response, Uri};
use hyper::body::Incoming;
use hyper::client::conn::http1::{Builder, SendRequest};
use hyper::rt::{Read, Write};

use crate::body::CountingBody;
use crate::executor::HEAD_BUFFER_BYTES;

/// hyper's sender for one HTTP/1.1 connection.
pub(super) struct Sender(SendRequest<CountingBody>);

/// Why a connection didn't answer a request.
pub(super) enum Refused {
    /// The request never reached the connection: it was closed, or hyper
    /// handed the request back before writing it. Boxed, because a request
    /// is far larger than an error.
    Unsent(Box<Request<CountingBody>>),
    /// The request failed after hyper took it.
    Failed(hyper::Error),
}

/// Runs hyper's HTTP/1.1 handshake on `io`, which writes nothing, and
/// spawns the connection's task.
pub(super) async fn handshake<I>(io: I) -> Result<Sender, hyper::Error>
where
    I: Read + Write + Send + Unpin + 'static,
{
    let (sender, connection) = Builder::new()
        .max_buf_size(HEAD_BUFFER_BYTES)
        .handshake(io)
        .await?;
    tokio::spawn(async move {
        // Each hop learns how the connection ended from its own request.
        let _ = connection.await;
    });
    Ok(Sender(sender))
}

impl Sender {
    /// Whether hyper has closed the connection.
    pub(super) fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// Waits until hyper is ready for a request: once the connection's task
    /// has finished the last exchange. It fails if the connection closes
    /// first.
    pub(super) async fn ready(&mut self) -> Result<(), hyper::Error> {
        self.0.ready().await
    }

    /// Sends `request`, a request already shaped by [`shape`], and waits for
    /// its head. A request refused because the connection is closed, or that
    /// hyper hands back unsent, comes back as [`Refused::Unsent`].
    pub(super) async fn send(
        &mut self,
        request: Request<CountingBody>,
    ) -> Result<Response<Incoming>, Refused> {
        if self.0.is_closed() {
            return Err(Refused::Unsent(Box::new(request)));
        }
        self.0
            .try_send_request(request)
            .await
            .map_err(|mut error| match error.take_message() {
                Some(request) => Refused::Unsent(Box::new(request)),
                None => Refused::Failed(error.into_error()),
            })
    }
}

/// Shapes `request` for HTTP/1.1 as hyper-util did. It adds `Host` when the
/// request has none: the URI's host, plus `:port` when the port isn't the
/// scheme's default, 443 for `https` and 80 otherwise. It then puts the
/// target in origin form, the path and query. `prepare` refuses `CONNECT`,
/// so no request needs authority form.
pub(super) fn shape<B>(request: &mut Request<B>) {
    if !request.headers().contains_key(HOST)
        && let Some(host) = host(request.uri())
    {
        // `prepare` caps the headers far below the map's limit, so the
        // insert can't fail.
        let _ = request.headers_mut().try_insert(HOST, host);
    }
    let target = match request.uri().path_and_query() {
        Some(target) if target.as_str() != "/" => Uri::from(target.clone()),
        _ => Uri::default(),
    };
    *request.uri_mut() = target;
}

/// The `Host` for `uri`, or `None` for a URI with no host.
fn host(uri: &Uri) -> Option<HeaderValue> {
    let host = uri.host()?;
    let default = if uri.scheme_str() == Some("https") {
        443
    } else {
        80
    };
    let value = match uri.port() {
        Some(port) if port.as_u16() != default => format!("{host}:{}", port.as_str()),
        _ => host.to_owned(),
    };
    HeaderValue::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use http::Request;
    use hyper_util::rt::TokioIo;

    use super::*;
    use crate::body::CountingBody;

    fn shaped(uri: &str, host: Option<&str>) -> (String, Option<String>) {
        let mut request = Request::get(uri);
        if let Some(host) = host {
            request = request.header(HOST, host);
        }
        let mut request = request.body(()).unwrap();
        shape(&mut request);
        let host = request
            .headers()
            .get(HOST)
            .map(|host| host.to_str().unwrap().to_owned());
        (request.uri().to_string(), host)
    }

    #[test]
    fn host_names_the_port_only_when_it_isnt_the_schemes_default() {
        let host = |uri| shaped(uri, None).1.unwrap();
        assert_eq!(host("http://a.test/"), "a.test");
        assert_eq!(host("http://a.test:80/"), "a.test");
        assert_eq!(host("https://a.test:443/"), "a.test");
        assert_eq!(host("http://a.test:443/"), "a.test:443");
        assert_eq!(host("https://a.test:80/"), "a.test:80");
        assert_eq!(host("http://[::1]:8080/"), "[::1]:8080");
        assert_eq!(
            shaped("http://a.test/", Some("own.test")).1.unwrap(),
            "own.test"
        );
    }

    #[test]
    fn the_target_is_in_origin_form() {
        assert_eq!(shaped("http://a.test:8080/p?q=1", None).0, "/p?q=1");
        assert_eq!(shaped("https://a.test/", None).0, "/");
    }

    /// The server half is gone before hyper's first read, which comes before
    /// it first asks for a request, so the connection closes without ever
    /// being ready, and `ready` ends once hyper has seen the close.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_connection_closed_before_its_request_leaves_it_unsent() {
        let (ours, theirs) = tokio::io::duplex(64);
        drop(theirs);
        let mut sender = handshake(TokioIo::new(ours))
            .await
            .expect("an HTTP/1.1 handshake writes nothing");
        assert!(sender.ready().await.is_err(), "the connection closed");
        assert!(sender.is_closed());

        let request = Request::get("/kept")
            .body(CountingBody::new(Bytes::new(), Arc::default()))
            .unwrap();
        match sender.send(request).await {
            Err(Refused::Unsent(request)) => assert_eq!(request.uri(), "/kept"),
            Err(Refused::Failed(_)) => panic!("the request failed instead"),
            Ok(_) => panic!("the request was answered"),
        }
    }
}
