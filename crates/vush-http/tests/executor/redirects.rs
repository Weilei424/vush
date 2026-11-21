//! Redirects followed by core's `run_attempt` through the real executor.

use vush_core::plan::HttpVersion;
use vush_core::protocol::http::{HttpExecutor, HttpRequest};
use vush_core::protocol::{FailureKind, FailureStage, Outcome};
use vush_core::runner::{AttemptResult, run_attempt};
use vush_http::Executor;

use crate::common::{
    Echo, MIB, ReplyFuture, TIMEOUT, echo, executor, get, json_a_1, limited, method, pairs,
    redirecting, reply, spawn_h1,
};

async fn attempt(executor: &Executor, request: HttpRequest, redirects: u32) -> AttemptResult {
    let mut session = executor.new_session();
    run_attempt(executor, &mut session, request, TIMEOUT, redirects).await
}

fn status(result: &AttemptResult) -> u16 {
    result.response.as_ref().expect("a response").status
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chain_is_followed_hop_by_hop() {
    let addr = spawn_h1(redirecting(&[
        ("/a", 302, "/b"),
        ("/b", 302, "/c"),
        ("/c", 200, ""),
    ]))
    .await;
    let executor = executor(HttpVersion::Auto);
    let result = attempt(&executor, get(format!("http://{addr}/a")), 5).await;
    assert_eq!(result.record.hops().len(), 3);
    assert_eq!(status(&result), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn post_becomes_get_on_302() {
    let addr = spawn_h1(redirecting(&[("/form", 302, "/echo")])).await;
    let executor = executor(HttpVersion::Auto);
    let mut request = get(format!("http://{addr}/form"));
    request.method = method("POST");
    request.body = Some(json_a_1());
    let result = attempt(&executor, request, 5).await;
    let echo = Echo::of_response(result.response.as_ref().expect("a response"));
    assert_eq!(echo.method, "GET");
    assert!(echo.values("content-type").is_empty());
    assert!(echo.body.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_307_keeps_method_and_body() {
    let addr = spawn_h1(redirecting(&[("/form", 307, "/echo")])).await;
    let executor = executor(HttpVersion::Auto);
    let mut request = get(format!("http://{addr}/form"));
    request.method = method("POST");
    request.body = Some(json_a_1());
    let result = attempt(&executor, request, 5).await;
    let echo = Echo::of_response(result.response.as_ref().expect("a response"));
    assert_eq!(echo.method, "POST");
    assert_eq!(echo.body, br#"{"a":1}"#);
    assert_eq!(echo.values("content-type"), vec!["application/json"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loop_ends_with_too_many_redirects() {
    let addr = spawn_h1(redirecting(&[("/loop", 302, "/loop")])).await;
    let executor = executor(HttpVersion::Auto);
    let result = attempt(&executor, get(format!("http://{addr}/loop")), 3).await;
    assert_eq!(result.record.hops().len(), 4);
    assert_eq!(
        result.record.outcome(),
        Outcome::Failed {
            kind: FailureKind::TooManyRedirects,
            stage: FailureStage::Prepare
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn redirects_zero_returns_the_302() {
    let addr = spawn_h1(redirecting(&[("/a", 302, "/b")])).await;
    let executor = executor(HttpVersion::Auto);
    let result = attempt(&executor, get(format!("http://{addr}/a")), 0).await;
    assert_eq!(result.record.hops().len(), 1);
    assert_eq!(status(&result), 302);
}

#[tokio::test(flavor = "multi_thread")]
async fn credentials_stay_behind_on_another_origin() {
    let b = spawn_h1(echo).await;
    let location = format!("http://{b}/echo");
    let a = spawn_h1(redirecting(&[("/start", 302, location.as_str())])).await;
    let executor = executor(HttpVersion::Auto);
    let mut request = get(format!("http://{a}/start"));
    request.headers = pairs(&[
        ("Authorization", "Bearer t"),
        ("Cookie", "c=1"),
        ("X-Keep", "1"),
    ]);
    let result = attempt(&executor, request, 5).await;
    assert_eq!(result.record.hops().len(), 2);
    let echo = Echo::of_response(result.response.as_ref().expect("a response"));
    assert_eq!(echo.values("x-keep"), vec!["1"]);
    assert!(echo.values("authorization").is_empty());
    assert!(echo.values("cookie").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_location_ends_with_invalid_redirect() {
    let addr = spawn_h1(redirecting(&[("/a", 302, "ftp://h/")])).await;
    let executor = executor(HttpVersion::Auto);
    let result = attempt(&executor, get(format!("http://{addr}/a")), 5).await;
    assert_eq!(result.record.hops().len(), 1);
    assert_eq!(
        result.record.outcome(),
        Outcome::Failed {
            kind: FailureKind::InvalidRedirect,
            stage: FailureStage::Prepare
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_redirect_body_keeps_its_connection() {
    let addr = spawn_h1(|request| -> ReplyFuture {
        if request.uri().path() == "/big" {
            Box::pin(
                async move { reply(302, &[("location", "/echo")], vec![b'x'; 2 * MIB as usize]) },
            )
        } else {
            echo(request)
        }
    })
    .await;
    let executor = limited(HttpVersion::Http1, MIB);
    let result = attempt(&executor, get(format!("http://{addr}/big")), 5).await;
    let hops = result.record.hops();
    assert_eq!(hops.len(), 2);
    assert_eq!(hops[0].sizes.received, 2 * MIB);
    assert!(hops[0].connection.is_some());
    assert_eq!(hops[0].connection, hops[1].connection);
    assert_eq!(status(&result), 200);
}
