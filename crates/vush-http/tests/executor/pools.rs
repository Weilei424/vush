//! Whose connections a session uses: the executor's pool, or under
//! `pool: vu` the user's own. That an executor refuses another's session.
//! And which scopes of a plan share a pool: only `shared` ones with the same
//! version and trust.

use std::path::Path;
use std::time::Duration;

use rustls::RootCertStore;
use vush_core::plan::{HttpSettings, HttpVersion, PoolScope, TlsSettings, WorkloadPlan};
use vush_core::protocol::http::{HttpExecutor, HttpHop, HttpRequest};
use vush_core::protocol::{FailureKind, FailureStage};
use vush_http::{BuildError, CaFileProblem, Executor, Executors, Scope, Session};

use crate::common::{
    CountingListener, LOCALHOST, MIB, NO_DIR, TIMEOUT, TempDir, TestCa, echo, failure, get, plan,
    response, send, send_within, spawn_h1, spawn_tls,
};

const FOREIGN: &str = "the session belongs to another executor";
const H2_AND_H1: &[&[u8]] = &[b"h2", b"http/1.1"];

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

// ---- A plan's executors ----

/// A plan's executors, trusting no certificate, for plaintext servers.
fn built(plan: &WorkloadPlan) -> Executors {
    Executors::with_roots(plan, Path::new(NO_DIR), RootCertStore::empty()).unwrap()
}

fn pool(pool: PoolScope) -> HttpSettings {
    HttpSettings {
        pool,
        ..HttpSettings::default()
    }
}

fn tls(ca_file: Option<&str>, insecure: bool) -> HttpSettings {
    HttpSettings {
        tls: TlsSettings {
            ca_file: ca_file.map(str::to_owned),
            insecure,
        },
        ..HttpSettings::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_scopes_with_one_profile_reuse_a_connection() {
    let addr = spawn_h1(echo).await;
    let request = get(format!("http://{addr}/"));
    let default = HttpSettings::default();
    let executors = built(&plan(default.clone(), vec![default.clone(), default]));
    let mut connections = Vec::new();
    for executor in [
        executors.file(),
        executors.scenario(0).unwrap(),
        executors.scenario(1).unwrap(),
    ] {
        let hop = send(executor, &request).await;
        response(&hop);
        connections.push(hop.connection);
    }
    assert!(connections[0].is_some());
    assert_eq!(connections, [connections[0]; 3]);
    assert!(executors.scenario(2).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_scopes_with_different_versions_do_not_share() {
    let addr = spawn_h1(echo).await;
    let request = get(format!("http://{addr}/"));
    let http1 = HttpSettings {
        version: HttpVersion::Http1,
        ..HttpSettings::default()
    };
    let default = HttpSettings::default();
    let executors = built(&plan(default.clone(), vec![default, http1]));
    let auto = send(executors.scenario(0).unwrap(), &request).await;
    response(&auto);
    let only_http1 = send(executors.scenario(1).unwrap(), &request).await;
    response(&only_http1);
    assert!(auto.connection.is_some());
    assert_ne!(auto.connection, only_http1.connection);
}

/// A connection made without verification never serves settings that
/// verify, and the other way round, whichever comes first.
#[tokio::test(flavor = "multi_thread")]
async fn shared_pools_keep_trust_apart() {
    let ca = TestCa::generate();
    let addr = spawn_tls(echo, H2_AND_H1, None, ca.leaf_for_ip(LOCALHOST)).await;
    let request = get(format!("https://{addr}/"));
    let dir = TempDir::new();
    dir.write("ca.pem", ca.pem().as_bytes());
    let scenarios = vec![tls(Some("ca.pem"), false), tls(None, true)];
    for order in [[0, 1], [1, 0]] {
        let plan = plan(HttpSettings::default(), scenarios.clone());
        let executors = Executors::with_roots(&plan, dir.path(), RootCertStore::empty()).unwrap();
        let mut connections = Vec::new();
        for index in order {
            let hop = send(executors.scenario(index).unwrap(), &request).await;
            assert_eq!(response(&hop).status, 200, "scenario {index} of {order:?}");
            connections.push(hop.connection);
        }
        assert!(connections[0].is_some(), "{order:?}");
        assert!(connections[1].is_some(), "{order:?}");
        assert_ne!(connections[0], connections[1], "{order:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn scenario_pools_are_their_own() {
    let addr = spawn_h1(echo).await;
    let request = get(format!("http://{addr}/"));
    let file = pool(PoolScope::Scenario);
    let executors = built(&plan(file.clone(), vec![file.clone(), file]));
    let mut connections = Vec::new();
    for executor in [
        executors.file(),
        executors.scenario(0).unwrap(),
        executors.scenario(1).unwrap(),
    ] {
        let hop = send(executor, &request).await;
        response(&hop);
        assert!(hop.connection.is_some());
        connections.push(hop.connection);
    }
    assert_ne!(connections[0], connections[1]);
    assert_ne!(connections[0], connections[2]);
    assert_ne!(connections[1], connections[2]);

    let again = send(executors.scenario(0).unwrap(), &request).await;
    response(&again);
    assert_eq!(again.connection, connections[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vu_scope_never_joins_the_shared_pool() {
    let addr = spawn_h1(echo).await;
    let request = get(format!("http://{addr}/"));
    let default = HttpSettings::default();
    let executors = built(&plan(default.clone(), vec![default, pool(PoolScope::Vu)]));
    let shared = send(executors.scenario(0).unwrap(), &request).await;
    response(&shared);
    let file = send(executors.file(), &request).await;
    response(&file);
    let user = send(executors.scenario(1).unwrap(), &request).await;
    response(&user);
    assert!(shared.connection.is_some());
    assert_eq!(file.connection, shared.connection);
    assert_ne!(user.connection, shared.connection);
}

#[test]
fn ca_file_errors_name_the_first_scope_that_reads_it() {
    let dir = TempDir::new();
    let dir_text = dir.path().to_str().unwrap();
    let default = HttpSettings::default();
    let missing = tls(Some("missing.pem"), false);
    let skipped = tls(Some("missing.pem"), true);
    let in_scenario_1 = "`scenarios[1].http.tls.ca_file` can't be used: the file doesn't exist";
    let in_file = "`http.tls.ca_file` can't be used: the file doesn't exist";
    let cases = [
        (
            "a",
            plan(default.clone(), vec![default, missing.clone()]),
            Scope::Scenario(1),
            in_scenario_1,
        ),
        (
            "b",
            plan(HttpSettings::default(), vec![skipped, missing.clone()]),
            Scope::Scenario(1),
            in_scenario_1,
        ),
        (
            "c",
            plan(missing.clone(), vec![missing.clone(), missing]),
            Scope::File,
            in_file,
        ),
    ];
    for (case, plan, at, text) in cases {
        let error = Executors::with_roots(&plan, dir.path(), RootCertStore::empty())
            .err()
            .expect("the ca_file is missing");
        assert!(
            matches!(
                error,
                BuildError::CaFile {
                    at: found,
                    problem: CaFileProblem::NotFound
                } if found == at
            ),
            "({case}) {error:?}"
        );
        assert_eq!(error.to_string(), text, "({case})");
        let shown = format!("{error} {error:?}");
        for secret in ["missing", dir_text] {
            assert!(!shown.contains(secret), "({case}) {shown}");
        }
    }
}

/// A pool's connect limit is the longest attempt timeout it serves, here
/// 200 ms, which is shorter than the hop's time, so the connect's own limit
/// ends the hop.
#[tokio::test(flavor = "multi_thread")]
async fn background_connects_stop_at_the_pool_limit() {
    let listener = CountingListener::bind().await;
    let quick = HttpSettings {
        timeout: Duration::from_millis(200),
        ..HttpSettings::default()
    };
    let executors = built(&plan(quick.clone(), vec![quick.clone(), quick]));
    // The kernel completes the TCP connect and nobody answers the handshake.
    let hop = send_within(
        executors.scenario(0).unwrap(),
        &get(format!("https://{}/", listener.addr)),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Connect));
    assert!(
        hop.timing.total < Duration::from_secs(5),
        "{:?}",
        hop.timing
    );
}

/// Scopes that share a pool share one connect limit, the longest of theirs:
/// here 2 s, not the 200 ms of the file level and scenario 0.
#[tokio::test(flavor = "multi_thread")]
async fn a_shared_pool_takes_the_longest_limit() {
    let listener = CountingListener::bind().await;
    let quick = HttpSettings {
        timeout: Duration::from_millis(200),
        ..HttpSettings::default()
    };
    let slow = HttpSettings {
        timeout: Duration::from_secs(2),
        ..HttpSettings::default()
    };
    let executors = built(&plan(quick.clone(), vec![quick, slow]));
    // The kernel completes the TCP connect and nobody answers the handshake.
    let hop = send_within(
        executors.scenario(0).unwrap(),
        &get(format!("https://{}/", listener.addr)),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Connect));
    assert!(
        hop.timing.total >= Duration::from_secs(1),
        "{:?}",
        hop.timing
    );
    assert!(
        hop.timing.total < Duration::from_secs(5),
        "{:?}",
        hop.timing
    );
}

#[test]
fn executors_build_outside_a_runtime() {
    let default = HttpSettings::default();
    let scenarios = vec![
        default.clone(),
        pool(PoolScope::Scenario),
        pool(PoolScope::Vu),
    ];
    let executors = built(&plan(default, scenarios));
    for executor in [
        executors.file(),
        executors.scenario(0).unwrap(),
        executors.scenario(1).unwrap(),
        executors.scenario(2).unwrap(),
    ] {
        let _session = executor.new_session();
    }

    let insecure = tls(None, true);
    let plan = plan(insecure.clone(), vec![insecure.clone(), insecure]);
    if let Err(error) = Executors::new(&plan, Path::new(NO_DIR)) {
        panic!("an insecure plan should build, but got {error:?}");
    }
}
