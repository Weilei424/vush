//! The request body, which counts its bytes as hyper sends them.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};

/// An encoded request body in one frame. When hyper pulls the frame, its
/// length is added to `sent`, so a hop that fails before the body went out
/// reports 0.
pub(crate) struct CountingBody {
    data: Option<Bytes>,
    sent: Arc<AtomicU64>,
}

impl CountingBody {
    /// A body of `data`. An empty `data` is a body with no frames.
    pub(crate) fn new(data: Bytes, sent: Arc<AtomicU64>) -> Self {
        Self {
            data: (!data.is_empty()).then_some(data),
            sent,
        }
    }
}

impl Body for CountingBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        Poll::Ready(this.data.take().map(|data| {
            this.sent.fetch_add(data.len() as u64, Ordering::Relaxed);
            Ok(Frame::data(data))
        }))
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.data.as_ref().map_or(0, |data| data.len() as u64))
    }
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;

    use super::*;

    #[tokio::test]
    async fn the_body_is_counted_once_as_it_is_pulled() {
        let sent = Arc::new(AtomicU64::new(0));
        let mut body = CountingBody::new(Bytes::from_static(b"{\"a\":1}"), sent.clone());
        assert_eq!(body.size_hint().exact(), Some(7));
        assert!(!body.is_end_stream());
        assert_eq!(sent.load(Ordering::Relaxed), 0);

        let frame = body.frame().await.unwrap().unwrap();
        assert_eq!(frame.into_data().unwrap(), b"{\"a\":1}".as_slice());
        assert_eq!(sent.load(Ordering::Relaxed), 7);
        assert!(body.is_end_stream());
        assert_eq!(body.size_hint().exact(), Some(0));
        assert!(body.frame().await.is_none());
        assert_eq!(sent.load(Ordering::Relaxed), 7);
    }

    #[tokio::test]
    async fn an_empty_body_has_no_frames() {
        let sent = Arc::new(AtomicU64::new(0));
        let mut body = CountingBody::new(Bytes::new(), sent.clone());
        assert!(body.is_end_stream());
        assert_eq!(body.size_hint().exact(), Some(0));
        assert!(body.frame().await.is_none());
        assert_eq!(sent.load(Ordering::Relaxed), 0);
    }
}
