//! The trait an HTTP executor implements.

use std::time::Duration;

use crate::protocol::Failure;
use crate::protocol::http::{HttpHop, HttpRequest, HttpResponse};

/// Sends HTTP requests for core, one hop at a time. `vush-http` implements it.
///
/// Core calls executors through generics (`E: HttpExecutor`). The trait is not
/// dyn-compatible, and that is intended. It has no constructor, because
/// `vush-http` builds each executor from the plan's settings (response-size
/// limit, HTTP version, TLS, cookies, pool scope), so everything that reads
/// `tls.ca_file` or builds pools stays out of core. It doesn't require `Debug`
/// on [`Session`](HttpExecutor::Session), so core can't print one.
///
/// Every executor follows this contract:
///
/// 1. **One hop per call.** `send` sends exactly one request on the wire, or
///    none when it fails in `prepare`. It never follows a redirect and never
///    retries.
/// 2. **Finite time.** `send` finishes within `timeout`, which covers
///    connecting, sending, the response head and the body. A hop that runs out
///    of time ends with a `timeout` failure. A zero `timeout` fails with
///    `timeout` in `prepare`, without sending anything.
/// 3. **No panics on input.** Nothing a server or the network does, and
///    nothing in a request (for example a header value holding `\r\n`), makes
///    `send` or `follow` panic. Every problem comes back as a [`Failure`].
/// 4. **Cancel by drop.** Dropping the future cancels the hop, and the
///    executor and the session stay usable.
/// 5. **No per-user state in the executor.** Cookies and anything else per
///    user live in the session. `send` stores every `Set-Cookie` from the
///    response in the session, including on a `3xx`. Under `cookies: false`,
///    the session keeps none.
/// 6. **Sessions.** `new_session` makes the state for one user or one hook
///    run. A copy made with `clone` has its own cookies, while a pool held for
///    `pool: vu` may be shared between copies. Core relies on this for
///    `reset_each_iteration`, by restoring a copy taken after `vu_setup`. A
///    session must only be used with the executor that made it, because under
///    `pool: vu` it holds connections built with that executor's settings.
/// 7. **`follow` is pure.** It does no I/O and takes no session. It returns
///    `Ok(Some(next))` when the response is a redirect it follows, `Ok(None)`
///    when it isn't one, and `Err` with kind `invalid_redirect` and stage
///    `prepare` when it is a redirect that can't be followed; core then ends
///    the attempt with that failure. A `3xx` without a `Location`, such as a
///    `304`, isn't a redirect to follow, so it gives `Ok(None)`. The `next`
///    request has an `Absolute` URL and an empty `query`. `follow` owns the
///    HTTP redirect rules: resolving `Location` against the request URL,
///    switching the method to `GET` and dropping the body where HTTP says so,
///    and dropping credentials when the origin changes. It never counts
///    redirects, because core does. When `http.redirects` is 1 or more and a
///    chain needs more redirects than that, core ends the attempt with
///    `too_many_redirects` in `prepare`. With `redirects: 0` nothing is
///    followed: a `3xx` is the attempt's response.
/// 8. **Safe text.** Failures carry fixed messages only. A failure's message
///    never contains a URL, a header value, body bytes or library error text.
pub trait HttpExecutor: Send + Sync + 'static {
    /// One user's protocol state: cookies, and the user's own pool under
    /// `pool: vu`.
    ///
    /// Cloning a session copies its cookies, so the copy changes
    /// independently. A `pool: vu` pool may be shared between copies.
    type Session: Clone + Send + 'static;

    /// Makes the state for one user or one hook run (contract point 6).
    fn new_session(&self) -> Self::Session;

    /// Sends one request and returns its hop (contract points 1 to 5 and 8).
    ///
    /// An implementation may write `async fn send(&self, …) -> HttpHop`, as
    /// the test fake does. The future must still be `Send`.
    fn send(
        &self,
        session: &mut Self::Session,
        request: &HttpRequest,
        timeout: Duration,
    ) -> impl Future<Output = HttpHop> + Send;

    /// Decides whether `response` is a redirect, and what to send next
    /// (contract points 3, 7 and 8). Pure: no I/O, no session, no counting.
    /// On `Err`, core ends the attempt with
    /// [`AttemptRecord::end_with`](crate::protocol::AttemptRecord::end_with).
    fn follow(
        &self,
        request: &HttpRequest,
        response: &HttpResponse,
    ) -> Result<Option<HttpRequest>, Failure>;
}
