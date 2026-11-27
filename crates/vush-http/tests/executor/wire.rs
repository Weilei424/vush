//! What a server receives from the executor on each HTTP version: the
//! version, the URI, and every header in arrival order.
//!
//! These tests were recorded from the executor before its own connection
//! pool replaced hyper-util's client, so they prove the wire didn't change.

use vush_core::plan::HttpVersion;
use vush_core::protocol::http::{HttpBody, HttpRequest};
use vush_http::Executor;

use crate::common::{
    LOCALHOST, TestCa, Wire, executor, get, json_a_1, method, send, spawn_auto, spawn_h1,
    spawn_tls, trusting, wire,
};

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn post(url: String, body: Option<HttpBody>) -> HttpRequest {
    HttpRequest {
        method: method("POST"),
        body,
        ..get(url)
    }
}

/// Sends the four requests and gives what the server received for each.
async fn four(executor: &Executor, base: &str) -> Vec<Wire> {
    let mut seen = Vec::new();

    let hop = send(executor, &get(format!("{base}/p?q=1"))).await;
    seen.push(Wire::of(&hop));

    let hop = send(executor, &post(format!("{base}/p"), Some(json_a_1()))).await;
    assert_eq!(hop.sizes.sent, 7);
    seen.push(Wire::of(&hop));

    let hop = send(executor, &post(format!("{base}/p"), None)).await;
    assert_eq!(hop.sizes.sent, 0);
    seen.push(Wire::of(&hop));

    let mut request = get(format!("{base}/p"));
    request.headers = vec![("Host".to_owned(), "example.test".to_owned())];
    let hop = send(executor, &request).await;
    seen.push(Wire::of(&hop));
    seen
}

#[tokio::test(flavor = "multi_thread")]
async fn http1_requests_arrive_as_today() {
    let addr = spawn_h1(wire).await;
    let seen = four(&executor(HttpVersion::Http1), &format!("http://{addr}")).await;
    let expected = vec![
        Wire {
            version: "HTTP/1.1".to_owned(),
            method: "GET".to_owned(),
            uri: "/p?q=1".to_owned(),
            headers: strings(&["user-agent: vush/0.1.0", "host: 127.0.0.1:PORT"]),
            body: b"".to_vec(),
        },
        Wire {
            version: "HTTP/1.1".to_owned(),
            method: "POST".to_owned(),
            uri: "/p".to_owned(),
            headers: strings(&[
                "content-type: application/json",
                "user-agent: vush/0.1.0",
                "host: 127.0.0.1:PORT",
                "content-length: 7",
            ]),
            body: br#"{"a":1}"#.to_vec(),
        },
        Wire {
            version: "HTTP/1.1".to_owned(),
            method: "POST".to_owned(),
            uri: "/p".to_owned(),
            headers: strings(&["user-agent: vush/0.1.0", "host: 127.0.0.1:PORT"]),
            body: b"".to_vec(),
        },
        Wire {
            version: "HTTP/1.1".to_owned(),
            method: "GET".to_owned(),
            uri: "/p".to_owned(),
            headers: strings(&["host: example.test", "user-agent: vush/0.1.0"]),
            body: b"".to_vec(),
        },
    ];
    assert_eq!(seen, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn http2_requests_arrive_as_today() {
    let addr = spawn_auto(wire).await;
    let seen = four(&executor(HttpVersion::Http2), &format!("http://{addr}")).await;
    let expected = vec![
        Wire {
            version: "HTTP/2.0".to_owned(),
            method: "GET".to_owned(),
            uri: "http://127.0.0.1:PORT/p?q=1".to_owned(),
            headers: strings(&["user-agent: vush/0.1.0"]),
            body: b"".to_vec(),
        },
        Wire {
            version: "HTTP/2.0".to_owned(),
            method: "POST".to_owned(),
            uri: "http://127.0.0.1:PORT/p".to_owned(),
            headers: strings(&[
                "content-type: application/json",
                "user-agent: vush/0.1.0",
                "content-length: 7",
            ]),
            body: br#"{"a":1}"#.to_vec(),
        },
        Wire {
            version: "HTTP/2.0".to_owned(),
            method: "POST".to_owned(),
            uri: "http://127.0.0.1:PORT/p".to_owned(),
            headers: strings(&["user-agent: vush/0.1.0", "content-length: 0"]),
            body: b"".to_vec(),
        },
        Wire {
            version: "HTTP/2.0".to_owned(),
            method: "GET".to_owned(),
            uri: "http://127.0.0.1:PORT/p".to_owned(),
            headers: strings(&["host: example.test", "user-agent: vush/0.1.0"]),
            body: b"".to_vec(),
        },
    ];
    assert_eq!(seen, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_over_tls_requests_arrive_as_today() {
    let ca = TestCa::generate();
    let addr = spawn_tls(wire, &[b"h2", b"http/1.1"], None, ca.leaf_for_ip(LOCALHOST)).await;
    let seen = four(
        &trusting(&ca, HttpVersion::Auto),
        &format!("https://{addr}"),
    )
    .await;
    let expected = vec![
        Wire {
            version: "HTTP/2.0".to_owned(),
            method: "GET".to_owned(),
            uri: "https://127.0.0.1:PORT/p?q=1".to_owned(),
            headers: strings(&["user-agent: vush/0.1.0"]),
            body: b"".to_vec(),
        },
        Wire {
            version: "HTTP/2.0".to_owned(),
            method: "POST".to_owned(),
            uri: "https://127.0.0.1:PORT/p".to_owned(),
            headers: strings(&[
                "content-type: application/json",
                "user-agent: vush/0.1.0",
                "content-length: 7",
            ]),
            body: br#"{"a":1}"#.to_vec(),
        },
        Wire {
            version: "HTTP/2.0".to_owned(),
            method: "POST".to_owned(),
            uri: "https://127.0.0.1:PORT/p".to_owned(),
            headers: strings(&["user-agent: vush/0.1.0", "content-length: 0"]),
            body: b"".to_vec(),
        },
        Wire {
            version: "HTTP/2.0".to_owned(),
            method: "GET".to_owned(),
            uri: "https://127.0.0.1:PORT/p".to_owned(),
            headers: strings(&["host: example.test", "user-agent: vush/0.1.0"]),
            body: b"".to_vec(),
        },
    ];
    assert_eq!(seen, expected);
}
