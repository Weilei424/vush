//! Requests that get a response: what arrives at the server, which version
//! is spoken, what comes back, and how connections are shared.

use rustls::RootCertStore;
use vush_core::plan::HttpVersion;
use vush_core::protocol::http::{HttpBody, HttpExecutor, HttpRequest, RequestUrl, WireVersion};
use vush_core::protocol::{FailureKind, FailureStage};
use vush_http::Executor;

use crate::common::{
    Echo, LOCALHOST, MIB, TestCa, echo, executor, failure, get, head_with_length, header_values,
    json_a_1, method, no_content, pairs, raw_headers, redirecting, response, send, settings, sized,
    spawn_auto, spawn_h1, spawn_raw, spawn_tls, trusting,
};

const H2_AND_H1: &[&[u8]] = &[b"h2", b"http/1.1"];

#[tokio::test(flavor = "multi_thread")]
async fn echo_sees_the_exact_request() {
    let addr = spawn_h1(echo).await;
    let request = HttpRequest {
        method: method("GET"),
        url: RequestUrl::Joined {
            base: format!("http://{addr}/v1"),
            path: "/items".to_owned(),
        },
        query: pairs(&[("a b", "1")]),
        headers: pairs(&[("X-Test", "yes"), ("X-Rep", "1"), ("X-Rep", "2")]),
        body: None,
    };
    let hop = send(&executor(HttpVersion::Auto), &request).await;
    let got = response(&hop);
    assert_eq!(got.status, 200);
    assert_eq!(got.version, WireVersion::Http11);
    let echo = Echo::of(&hop);
    assert_eq!(echo.method, "GET");
    assert_eq!(echo.target, "/v1/items?a%20b=1");
    assert_eq!(echo.values("x-test"), ["yes"]);
    assert_eq!(echo.values("x-rep"), ["1", "2"]);
    assert_eq!(echo.values("user-agent"), ["vush/0.1.0"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn bodies_arrive_encoded_with_default_content_types() {
    let addr = spawn_h1(echo).await;
    let cases = [
        (json_a_1(), vec![], "{\"a\":1}", "application/json"),
        (
            HttpBody::Form(pairs(&[("a", "b c")])),
            vec![],
            "a=b+c",
            "application/x-www-form-urlencoded",
        ),
        (
            HttpBody::Text("hi".to_owned()),
            vec![],
            "hi",
            "text/plain; charset=utf-8",
        ),
        (
            HttpBody::Text("<a/>".to_owned()),
            pairs(&[("Content-Type", "application/xml")]),
            "<a/>",
            "application/xml",
        ),
    ];
    let executor = executor(HttpVersion::Auto);
    for (body, headers, wire, content_type) in cases {
        let mut request = get(format!("http://{addr}/echo"));
        request.method = method("POST");
        request.headers = headers;
        request.body = Some(body);
        let echo = Echo::of(&send(&executor, &request).await);
        assert_eq!(echo.method, "POST");
        assert_eq!(echo.body, wire.as_bytes());
        assert_eq!(echo.values("content-type"), [content_type], "{wire}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_methods_are_sent_as_written() {
    let addr = spawn_h1(echo).await;
    let mut request = get(format!("http://{addr}/cache"));
    request.method = method("PURGE");
    let echo = Echo::of(&send(&executor(HttpVersion::Auto), &request).await);
    assert_eq!(echo.method, "PURGE");
}

#[tokio::test(flavor = "multi_thread")]
async fn http_versions_follow_the_mode() {
    let plain = spawn_auto(echo).await;
    let hop = send(
        &executor(HttpVersion::Http2),
        &get(format!("http://{plain}/")),
    )
    .await;
    assert_eq!(response(&hop).version, WireVersion::Http2);

    let ca = TestCa::generate();
    let tls = spawn_tls(echo, H2_AND_H1, None, ca.leaf_for_ip(LOCALHOST)).await;
    let url = format!("https://{tls}/");
    for (version, wire) in [
        (HttpVersion::Auto, WireVersion::Http2),
        (HttpVersion::Http1, WireVersion::Http11),
    ] {
        let hop = send(&trusting(&ca, version), &get(url.clone())).await;
        assert_eq!(response(&hop).version, wire, "{version:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn http2_without_server_agreement_fails_as_tls() {
    let ca = TestCa::generate();
    let offers: [&[&[u8]]; 2] = [&[b"http/1.1"], &[]];
    for offer in offers {
        let addr = spawn_tls(echo, offer, None, ca.leaf_for_ip(LOCALHOST)).await;
        let hop = send(
            &trusting(&ca, HttpVersion::Http2),
            &get(format!("https://{addr}/")),
        )
        .await;
        assert_eq!(
            failure(&hop),
            (FailureKind::Tls, FailureStage::Connect),
            "{offer:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_trust_is_checked() {
    let ca = TestCa::generate();
    let trusted = spawn_tls(echo, H2_AND_H1, None, ca.leaf_for_ip(LOCALHOST)).await;
    let hop = send(
        &trusting(&ca, HttpVersion::Auto),
        &get(format!("https://{trusted}/")),
    )
    .await;
    assert_eq!(response(&hop).status, 200);

    let other = TestCa::generate();
    let hop = send(
        &trusting(&other, HttpVersion::Auto),
        &get(format!("https://{trusted}/")),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Tls, FailureStage::Connect));

    let wrong_name = spawn_tls(echo, H2_AND_H1, None, ca.leaf_for_name("other.test")).await;
    let hop = send(
        &trusting(&ca, HttpVersion::Auto),
        &get(format!("https://{wrong_name}/")),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Tls, FailureStage::Connect));
}

#[tokio::test(flavor = "multi_thread")]
async fn bodies_over_the_limit_are_cut() {
    let addr = spawn_h1(sized).await;
    let executor =
        Executor::with_roots(&settings(HttpVersion::Auto, MIB), RootCertStore::empty()).unwrap();
    for (path, truncated) in [
        ("/fixed/2097152", true),
        ("/chunked/2097152", true),
        ("/fixed/1048576", false),
        ("/chunked/1048576", false),
    ] {
        let hop = send(&executor, &get(format!("http://{addr}{path}"))).await;
        let got = response(&hop);
        assert_eq!(got.status, 200, "{path}");
        assert_eq!(got.truncated, truncated, "{path}");
        let size = path.rsplit('/').next().unwrap().as_bytes();
        assert_eq!(header_values(got, "x-size"), [size], "{path}");
        assert_eq!(got.body.len() as u64, MIB, "{path}");
        assert_eq!(hop.sizes.received, MIB, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn head_and_204_responses_have_empty_bodies() {
    let executor = executor(HttpVersion::Auto);

    let addr = spawn_raw(head_with_length).await;
    let mut request = get(format!("http://{addr}/"));
    request.method = method("HEAD");
    let hop = send(&executor, &request).await;
    let got = response(&hop);
    assert_eq!(got.status, 200);
    assert_eq!(header_values(got, "content-length"), [b"100"]);
    assert!(got.body.is_empty());
    assert!(!got.truncated);

    let addr = spawn_raw(no_content).await;
    let hop = send(&executor, &get(format!("http://{addr}/"))).await;
    let got = response(&hop);
    assert_eq!(got.status, 204);
    assert!(got.body.is_empty());
    assert!(!got.truncated);
}

#[tokio::test(flavor = "multi_thread")]
async fn response_headers_keep_repeats_and_raw_bytes() {
    let addr = spawn_raw(raw_headers).await;
    let hop = send(
        &executor(HttpVersion::Auto),
        &get(format!("http://{addr}/")),
    )
    .await;
    let got = response(&hop);
    assert_eq!(
        header_values(got, "set-cookie"),
        [b"a=1".as_slice(), b"b=2".as_slice()]
    );
    assert_eq!(header_values(got, "x-raw"), [b"\xff\xfe".as_slice()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn connections_are_reused_without_a_race() {
    let ca = TestCa::generate();

    // Only the first connection gets a handshake, so the second hop can only
    // succeed on the pooled one.
    let h1 = spawn_tls(echo, &[b"http/1.1"], Some(1), ca.leaf_for_ip(LOCALHOST)).await;
    let executor = trusting(&ca, HttpVersion::Http1);
    let url = format!("https://{h1}/");
    let first = send(&executor, &get(url.clone())).await;
    let second = send(&executor, &get(url)).await;
    assert_eq!(response(&first).version, WireVersion::Http11);
    assert_eq!(response(&second).status, 200);
    assert!(first.connection.is_some());
    assert_eq!(first.connection, second.connection);
    let setup = first
        .timing
        .setup
        .expect("a new connection reports its setup");
    assert!(setup.tls.is_some());
    assert_eq!(setup.dns, None);
    assert_eq!(second.timing.setup, None);

    let h2 = spawn_tls(echo, &[b"h2"], Some(1), ca.leaf_for_ip(LOCALHOST)).await;
    let executor = trusting(&ca, HttpVersion::Auto);
    let url = format!("https://{h2}/");
    let first = send(&executor, &get(url.clone())).await;
    let second = send(&executor, &get(url)).await;
    assert_eq!(response(&first).version, WireVersion::Http2);
    assert_eq!(response(&second).version, WireVersion::Http2);
    assert!(first.connection.is_some());
    assert_eq!(first.connection, second.connection);
    assert!(first.timing.setup.is_some());
    assert_eq!(second.timing.setup, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn ids_never_collide_across_executors() {
    let addr = spawn_h1(echo).await;
    let url = format!("http://{addr}/");
    let a = send(&executor(HttpVersion::Auto), &get(url.clone())).await;
    let b = send(&executor(HttpVersion::Auto), &get(url)).await;
    assert!(a.connection.is_some());
    assert!(b.connection.is_some());
    assert_ne!(a.connection, b.connection);
}

#[tokio::test(flavor = "multi_thread")]
async fn sent_sizes_count_the_encoded_body() {
    let addr = spawn_h1(echo).await;
    let executor = executor(HttpVersion::Auto);
    let mut post = get(format!("http://{addr}/"));
    post.method = method("POST");
    post.body = Some(json_a_1());
    let hop = send(&executor, &post).await;
    assert_eq!(Echo::of(&hop).body, b"{\"a\":1}");
    assert_eq!(hop.sizes.sent, 7);

    let hop = send(&executor, &get(format!("http://{addr}/"))).await;
    assert_eq!(response(&hop).status, 200);
    assert_eq!(hop.sizes.sent, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_executor_follows_with_the_redirect_rules() {
    let addr = spawn_h1(redirecting(&[("/old", 302, "/new?x=1")])).await;
    let executor = executor(HttpVersion::Auto);
    let mut post = get(format!("http://{addr}/old"));
    post.method = method("POST");
    post.body = Some(json_a_1());
    let hop = send(&executor, &post).await;
    let got = response(&hop);
    assert_eq!(got.status, 302);

    let next = executor
        .follow(&post, got)
        .unwrap()
        .expect("a 302 with a Location is followed");
    assert_eq!(
        next.url,
        RequestUrl::Absolute(format!("http://{addr}/new?x=1"))
    );
    let echo = Echo::of(&send(&executor, &next).await);
    assert_eq!(echo.method, "GET");
    assert_eq!(echo.target, "/new?x=1");
    assert!(echo.body.is_empty());
    assert!(echo.values("content-type").is_empty());

    let ok = response(&send(&executor, &get(format!("http://{addr}/plain"))).await).clone();
    assert_eq!(executor.follow(&next, &ok), Ok(None));
}
