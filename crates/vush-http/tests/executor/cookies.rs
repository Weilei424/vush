//! The cookie jar over real servers: what comes back, what stays with its
//! user, and the bounds. Every server is on 127.0.0.1.

use std::future::ready;
use std::path::Path;
use std::time::Duration;

use http::Request;
use hyper::body::Incoming;
use rustls::RootCertStore;
use vush_core::plan::{HttpSettings, HttpVersion, MAX_REQUEST_HEADERS, ResourceLimits};
use vush_core::protocol::http::{HttpExecutor, HttpHop, HttpRequest};
use vush_core::protocol::{FailureKind, FailureStage};
use vush_core::runner::run_attempt;
use vush_http::{Executor, Session};

use crate::common::{
    CountingListener, Echo, LOCALHOST, MIB, NO_DIR, ReplyFuture, TIMEOUT, TestCa,
    cookie_then_cut_body, echo, executor, failure, get, header_values, pairs, raw_cookie, reply,
    response, settings, spawn_auto, spawn_h1, spawn_raw, spawn_tls, trusting,
};

const H2_AND_H1: &[&[u8]] = &[b"h2", b"http/1.1"];

/// A path, its status and its headers.
type Answer<'a> = (&'a str, u16, &'a [(&'a str, &'a str)]);

/// Answers each listed path with its status and headers, and echoes every
/// other path.
fn cookie_routes(
    routes: &[Answer<'_>],
) -> impl Fn(Request<Incoming>) -> ReplyFuture + Send + Sync + 'static {
    type Route = (String, u16, Vec<(String, String)>);
    let routes: Vec<Route> = routes
        .iter()
        .map(|(path, status, headers)| {
            let headers = headers
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect();
            ((*path).to_owned(), *status, headers)
        })
        .collect();
    move |request| {
        let route = routes
            .iter()
            .find(|(path, _, _)| path == request.uri().path());
        match route {
            None => echo(request),
            Some((_, status, headers)) => {
                let headers: Vec<(&str, &str)> = headers
                    .iter()
                    .map(|(n, v)| (n.as_str(), v.as_str()))
                    .collect();
                let answer = reply(*status, &headers, "");
                Box::pin(ready(answer))
            }
        }
    }
}

/// The `cookie` header values the echo saw.
fn seen(hop: &HttpHop) -> Vec<String> {
    Echo::of(hop)
        .values("cookie")
        .into_iter()
        .map(str::to_owned)
        .collect()
}

async fn go(executor: &Executor, session: &mut Session, url: String) -> HttpHop {
    executor.send(session, &get(url), TIMEOUT).await
}

fn lone(settings: &HttpSettings, limits: &ResourceLimits) -> Executor {
    Executor::with_roots(settings, limits, Path::new(NO_DIR), RootCertStore::empty()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cookie_comes_back_on_the_next_request() {
    for version in [HttpVersion::Http1, HttpVersion::Http2] {
        let addr = spawn_auto(cookie_routes(&[("/set", 200, &[("set-cookie", "a=1")])])).await;
        let executor = executor(version);
        let mut session = executor.new_session();
        let set = go(&executor, &mut session, format!("http://{addr}/set")).await;
        assert_eq!(header_values(response(&set), "set-cookie"), [b"a=1"]);
        let next = go(&executor, &mut session, format!("http://{addr}/echo")).await;
        assert_eq!(seen(&next), ["a=1"], "{version:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cookie_set_on_a_302_reaches_the_next_hop() {
    let addr = spawn_h1(cookie_routes(&[(
        "/start",
        302,
        &[("set-cookie", "hop=1"), ("location", "/echo")],
    )]))
    .await;
    let executor = executor(HttpVersion::Auto);
    let mut session = executor.new_session();
    let result = run_attempt(
        &executor,
        &mut session,
        get(format!("http://{addr}/start")),
        TIMEOUT,
        5,
    )
    .await;
    assert_eq!(result.record.hops().len(), 2);
    let last = Echo::of_response(result.response.as_ref().expect("a response"));
    assert_eq!(last.values("cookie"), ["hop=1"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn users_on_one_http2_connection_never_see_each_other() {
    let addr = spawn_auto(cookie_routes(&[
        ("/set-a", 200, &[("set-cookie", "who=a")]),
        ("/set-b", 200, &[("set-cookie", "who=b")]),
    ]))
    .await;
    let executor = executor(HttpVersion::Http2);
    let mut a = executor.new_session();
    let mut b = executor.new_session();
    // One after the other, so the pool holds one HTTP/2 connection.
    response(&go(&executor, &mut a, format!("http://{addr}/set-a")).await);
    response(&go(&executor, &mut b, format!("http://{addr}/set-b")).await);

    let mut request_a: HttpRequest = get(format!("http://{addr}/echo"));
    request_a.headers = pairs(&[("Authorization", "Bearer a")]);
    let mut request_b: HttpRequest = get(format!("http://{addr}/echo"));
    request_b.headers = pairs(&[("Authorization", "Bearer b")]);
    let (hop_a, hop_b) = tokio::join!(
        executor.send(&mut a, &request_a, TIMEOUT),
        executor.send(&mut b, &request_b, TIMEOUT),
    );

    assert!(hop_a.connection.is_some());
    assert_eq!(hop_a.connection, hop_b.connection);
    assert_eq!(seen(&hop_a), ["who=a"]);
    assert_eq!(seen(&hop_b), ["who=b"]);
    assert_eq!(Echo::of(&hop_a).values("authorization"), ["Bearer a"]);
    assert_eq!(Echo::of(&hop_b).values("authorization"), ["Bearer b"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn users_on_one_http1_pool_never_see_each_other() {
    let addr = spawn_h1(cookie_routes(&[
        ("/set-a", 200, &[("set-cookie", "who=a")]),
        ("/set-b", 200, &[("set-cookie", "who=b")]),
    ]))
    .await;
    let executor = executor(HttpVersion::Http1);
    let mut a = executor.new_session();
    let mut b = executor.new_session();
    response(&go(&executor, &mut a, format!("http://{addr}/set-a")).await);
    response(&go(&executor, &mut b, format!("http://{addr}/set-b")).await);

    let mut request_a = get(format!("http://{addr}/echo"));
    request_a.headers = pairs(&[("Authorization", "Bearer a")]);
    let mut request_b = get(format!("http://{addr}/echo"));
    request_b.headers = pairs(&[("Authorization", "Bearer b")]);
    let hop_a = executor.send(&mut a, &request_a, TIMEOUT).await;
    let hop_b = executor.send(&mut b, &request_b, TIMEOUT).await;

    assert!(hop_a.connection.is_some());
    assert_eq!(hop_a.connection, hop_b.connection);
    assert_eq!(seen(&hop_a), ["who=a"]);
    assert_eq!(seen(&hop_b), ["who=b"]);
    assert_eq!(Echo::of(&hop_a).values("authorization"), ["Bearer a"]);
    assert_eq!(Echo::of(&hop_b).values("authorization"), ["Bearer b"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_keeps_its_own_cookies() {
    let addr = spawn_h1(cookie_routes(&[
        ("/one", 200, &[("set-cookie", "one=1")]),
        ("/two", 200, &[("set-cookie", "two=2")]),
        ("/three", 200, &[("set-cookie", "three=3")]),
    ]))
    .await;
    let executor = executor(HttpVersion::Auto);
    let mut one = executor.new_session();
    go(&executor, &mut one, format!("http://{addr}/one")).await;
    let mut copy = one.clone();
    go(&executor, &mut one, format!("http://{addr}/two")).await;
    go(&executor, &mut copy, format!("http://{addr}/three")).await;

    let copied = go(&executor, &mut copy, format!("http://{addr}/echo")).await;
    assert_eq!(seen(&copied), ["one=1; three=3"]);
    let original = go(&executor, &mut one, format!("http://{addr}/echo")).await;
    assert_eq!(seen(&original), ["one=1; two=2"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_jar_under_cookies_false_or_zero_max_cookies() {
    let off = HttpSettings {
        cookies: false,
        ..settings(HttpVersion::Auto, MIB)
    };
    let zero = ResourceLimits {
        max_cookies: 0,
        ..ResourceLimits::default()
    };
    let executors = [
        lone(&off, &ResourceLimits::default()),
        lone(&settings(HttpVersion::Auto, MIB), &zero),
    ];
    for executor in executors {
        let addr = spawn_h1(cookie_routes(&[("/set", 200, &[("set-cookie", "a=1")])])).await;
        let mut session = executor.new_session();
        go(&executor, &mut session, format!("http://{addr}/set")).await;
        let mut request = get(format!("http://{addr}/echo"));
        request.headers = pairs(&[("Cookie", "mine=1")]);
        let hop = executor.send(&mut session, &request, TIMEOUT).await;
        assert_eq!(seen(&hop), ["mine=1"]);
        let bare = go(&executor, &mut session, format!("http://{addr}/echo")).await;
        assert!(seen(&bare).is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_jar_drops_the_least_recently_used() {
    let addr = spawn_h1(cookie_routes(&[
        ("/set-a", 200, &[("set-cookie", "a=1")]),
        ("/x/set-b", 200, &[("set-cookie", "b=1; Path=/x")]),
        ("/set-c", 200, &[("set-cookie", "c=1")]),
    ]))
    .await;
    let limits = ResourceLimits {
        max_cookies: 2,
        ..ResourceLimits::default()
    };
    let executor = lone(&settings(HttpVersion::Auto, MIB), &limits);
    let mut session = executor.new_session();
    go(&executor, &mut session, format!("http://{addr}/set-a")).await;
    go(&executor, &mut session, format!("http://{addr}/x/set-b")).await;
    // Sends only `a` (`b` is for `/x`), so `b` is now the least recently used
    // though `a` is the oldest.
    let first = go(&executor, &mut session, format!("http://{addr}/echo")).await;
    assert_eq!(seen(&first), ["a=1"]);
    go(&executor, &mut session, format!("http://{addr}/set-c")).await;
    let last = go(&executor, &mut session, format!("http://{addr}/x/echo")).await;
    assert_eq!(seen(&last), ["a=1; c=1"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_requests_own_cookie_wins_on_names() {
    let addr = spawn_h1(cookie_routes(&[(
        "/set",
        200,
        &[("set-cookie", "a=jar"), ("set-cookie", "b=jar")],
    )]))
    .await;
    let executor = executor(HttpVersion::Auto);
    let mut session = executor.new_session();
    go(&executor, &mut session, format!("http://{addr}/set")).await;
    let mut request = get(format!("http://{addr}/echo"));
    request.headers = pairs(&[("Cookie", "a=req")]);
    let hop = executor.send(&mut session, &request, TIMEOUT).await;
    assert_eq!(seen(&hop), ["a=req; b=jar"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn secure_cookies_go_only_over_tls() {
    let ca = TestCa::generate();
    let routes = || {
        cookie_routes(&[
            (
                "/set",
                200,
                &[("set-cookie", "s=1; Secure"), ("set-cookie", "p=1")],
            ),
            ("/set-x", 200, &[("set-cookie", "x=1; Secure")]),
        ])
    };
    let tls = spawn_tls(routes(), H2_AND_H1, None, ca.leaf_for_ip(LOCALHOST)).await;
    let plain = spawn_h1(routes()).await;
    let executor = trusting(&ca, HttpVersion::Auto);
    let mut session = executor.new_session();

    go(&executor, &mut session, format!("https://{tls}/set")).await;
    let secure = go(&executor, &mut session, format!("https://{tls}/echo")).await;
    assert_eq!(seen(&secure), ["s=1; p=1"]);
    let insecure = go(&executor, &mut session, format!("http://{plain}/echo")).await;
    assert_eq!(seen(&insecure), ["p=1"]);

    // A `Secure` cookie set over plain HTTP is ignored.
    go(&executor, &mut session, format!("http://{plain}/set-x")).await;
    let after = go(&executor, &mut session, format!("https://{tls}/echo")).await;
    assert_eq!(seen(&after), ["s=1; p=1"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn servers_delete_cookies() {
    let addr = spawn_h1(cookie_routes(&[
        ("/set", 200, &[("set-cookie", "a=1"), ("set-cookie", "b=1")]),
        (
            "/del",
            200,
            &[
                ("set-cookie", "a=; Max-Age=0"),
                ("set-cookie", "b=; Expires=Thu, 01 Jan 1970 00:00:00 GMT"),
            ],
        ),
    ]))
    .await;
    let executor = executor(HttpVersion::Auto);
    let mut session = executor.new_session();
    go(&executor, &mut session, format!("http://{addr}/set")).await;
    let before = go(&executor, &mut session, format!("http://{addr}/echo")).await;
    assert_eq!(seen(&before), ["a=1; b=1"]);
    go(&executor, &mut session, format!("http://{addr}/del")).await;
    let after = go(&executor, &mut session, format!("http://{addr}/echo")).await;
    assert!(seen(&after).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cookie_from_a_failed_body_is_kept() {
    let cut = spawn_raw(cookie_then_cut_body).await;
    let next = spawn_h1(echo).await;
    let executor = executor(HttpVersion::Auto);
    let mut session = executor.new_session();
    let failed = go(&executor, &mut session, format!("http://{cut}/")).await;
    assert_eq!(failure(&failed).1, FailureStage::Body);
    let hop = go(&executor, &mut session, format!("http://{next}/echo")).await;
    assert_eq!(seen(&hop), ["late=1"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn cookie_bytes_come_back_as_sent() {
    let raw = spawn_raw(raw_cookie).await;
    let back = spawn_h1(|request: Request<Incoming>| {
        let value = request.headers().get("cookie").unwrap().as_bytes().to_vec();
        ready(reply(200, &[], value))
    })
    .await;
    let executor = executor(HttpVersion::Auto);
    let mut session = executor.new_session();
    response(&go(&executor, &mut session, format!("http://{raw}/")).await);
    let hop = go(&executor, &mut session, format!("http://{back}/")).await;
    assert_eq!(response(&hop).body, b"raw=\xff\xfe");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_past_the_header_cap_sends_nothing() {
    let listener = CountingListener::bind().await;
    let executor = executor(HttpVersion::Auto);
    let mut session = executor.new_session();

    let mut request = get(format!("http://{}/", listener.addr));
    request.headers = (0..=MAX_REQUEST_HEADERS)
        .map(|n| (format!("x-{n}"), "v".into()))
        .collect();
    let hop = executor
        .send(&mut session, &request, Duration::from_secs(5))
        .await;

    assert_eq!(
        failure(&hop),
        (FailureKind::InvalidRequest, FailureStage::Prepare)
    );
    let Err(failed) = &hop.outcome else {
        panic!("the hop should fail")
    };
    assert_eq!(failed.message, "the request has too many headers");
    assert_eq!(hop.sizes.sent, 0);
    assert_eq!(hop.connection, None);
    assert_eq!(listener.accepted().await, 0);
}
