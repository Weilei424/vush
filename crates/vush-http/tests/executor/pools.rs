//! Whose connections a session uses: the executor's pool, or under
//! `pool: vu` the user's own. And that an executor refuses another's session.

use std::path::Path;
use std::time::Duration;

use rustls::RootCertStore;
use vush_core::plan::{HttpSettings, PoolScope};
use vush_core::protocol::http::{HttpExecutor, HttpHop, HttpRequest};
use vush_core::protocol::{FailureKind, FailureStage};
use vush_http::{Executor, Session};

use crate::common::{
    CountingListener, MIB, NO_DIR, TIMEOUT, echo, failure, get, response, send_within, spawn_h1,
};

const FOREIGN: &str = "the session belongs to another executor";

fn pooled(pool: PoolScope) -> Executor {
    let settings = HttpSettings {
        pool,
        ..crate::common::settings(vush_core::plan::HttpVersion::Auto, MIB)
    };
    Executor::with_roots(&settings, Path::new(NO_DIR), RootCertStore::empty()).unwrap()
}

async fn send_on(
    executor: &Executor,
    session: &mut Session,
    request: &HttpRequest,
    timeout: Duration,
) -> HttpHop {
    executor.send(session, request, timeout).await
}

#[tokio::test(flavor = "multi_thread")]
async fn vu_sessions_have_their_own_connections() {
    let addr = spawn_h1(echo).await;
    let request = get(format!("http://{addr}/"));
    let executor = pooled(PoolScope::Vu);
    let mut one = executor.new_session();
    let mut two = executor.new_session();

    let first = send_on(&executor, &mut one, &request, TIMEOUT).await;
    response(&first);
    let other = send_on(&executor, &mut two, &request, TIMEOUT).await;
    response(&other);
    assert_ne!(first.connection, other.connection);

    let again = send_on(&executor, &mut one, &request, TIMEOUT).await;
    response(&again);
    assert_eq!(first.connection, again.connection);

    let mut copy = one.clone();
    let copied = send_on(&executor, &mut copy, &request, TIMEOUT).await;
    response(&copied);
    assert_eq!(first.connection, copied.connection);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_of_a_pooled_executor_share_its_pool() {
    for pool in [PoolScope::Shared, PoolScope::Scenario] {
        let addr = spawn_h1(echo).await;
        let request = get(format!("http://{addr}/"));
        let executor = pooled(pool);
        let mut one = executor.new_session();
        let mut two = executor.new_session();
        let first = send_on(&executor, &mut one, &request, TIMEOUT).await;
        response(&first);
        let second = send_on(&executor, &mut two, &request, TIMEOUT).await;
        response(&second);
        assert!(first.connection.is_some());
        assert_eq!(first.connection, second.connection, "{pool:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_from_another_executor_is_refused() {
    for pool in [PoolScope::Shared, PoolScope::Vu] {
        let listener = CountingListener::bind().await;
        let request = get(format!("http://{}/", listener.addr));
        let (a, b) = (pooled(pool), pooled(pool));
        let mut foreign = b.new_session();
        let hop = send_on(&a, &mut foreign, &request, TIMEOUT).await;
        assert_eq!(
            failure(&hop),
            (FailureKind::InvalidRequest, FailureStage::Prepare),
            "{pool:?}"
        );
        assert_eq!(hop.outcome.as_ref().unwrap_err().message, FOREIGN);
        assert_eq!(hop.connection, None);
        assert_eq!(hop.sizes.sent, 0);
        assert_eq!(hop.sizes.received, 0);
        assert_eq!(listener.accepted().await, 0, "{pool:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_session_is_refused_before_the_timeout_check() {
    let request = get("http://127.0.0.1:1/".to_owned());
    let (a, b) = (pooled(PoolScope::Shared), pooled(PoolScope::Shared));
    let mut foreign = b.new_session();
    let hop = a.send(&mut foreign, &request, Duration::ZERO).await;
    assert_eq!(
        failure(&hop),
        (FailureKind::InvalidRequest, FailureStage::Prepare)
    );
    // `send_within` makes its own session, so it is not the misuse.
    let own = send_within(&a, &request, Duration::ZERO).await;
    assert_eq!(failure(&own), (FailureKind::Timeout, FailureStage::Prepare));
}

#[test]
fn vu_sessions_build_outside_a_runtime() {
    let executor = pooled(PoolScope::Vu);
    let session = executor.new_session();
    let _copy = session.clone();
}
