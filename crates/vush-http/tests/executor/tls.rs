//! Trust: `tls.ca_file` adds to the system's roots, and `tls.insecure`
//! accepts any certificate for any name.

use std::path::Path;

use rustls::RootCertStore;
use vush_core::plan::{HttpSettings, HttpVersion, ResourceLimits};
use vush_http::{BuildError, CaFileProblem, Executor, Scope};

use crate::common::{
    LOCALHOST, MIB, NO_DIR, TempDir, TestCa, echo, get, response, send, settings, spawn_tls,
};

const H2_AND_H1: &[&[u8]] = &[b"h2", b"http/1.1"];

fn with_ca_file(ca_file: &str) -> HttpSettings {
    let mut settings = settings(HttpVersion::Auto, MIB);
    settings.tls.ca_file = Some(ca_file.to_owned());
    settings
}

#[tokio::test(flavor = "multi_thread")]
async fn ca_file_adds_to_the_system_roots() {
    let (a, b) = (TestCa::generate(), TestCa::generate());
    let signed_by_a = spawn_tls(echo, H2_AND_H1, None, a.leaf_for_ip(LOCALHOST)).await;
    let signed_by_b = spawn_tls(echo, H2_AND_H1, None, b.leaf_for_ip(LOCALHOST)).await;
    let dir = TempDir::new();
    dir.write("b.pem", b.pem().as_bytes());
    let executor = Executor::with_roots(
        &with_ca_file("b.pem"),
        &ResourceLimits::default(),
        dir.path(),
        a.roots(),
    )
    .unwrap();
    for addr in [signed_by_a, signed_by_b] {
        let hop = send(&executor, &get(format!("https://{addr}/"))).await;
        assert_eq!(response(&hop).status, 200, "{addr}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_absolute_ca_file_ignores_the_test_dir() {
    let (a, b) = (TestCa::generate(), TestCa::generate());
    let signed_by_b = spawn_tls(echo, H2_AND_H1, None, b.leaf_for_ip(LOCALHOST)).await;
    let dir = TempDir::new();
    let path = dir.write("b.pem", b.pem().as_bytes());
    assert!(path.is_absolute(), "{path:?}");
    let other = TempDir::new();
    let settings = with_ca_file(path.to_str().unwrap());
    let executor = Executor::with_roots(
        &settings,
        &ResourceLimits::default(),
        other.path(),
        a.roots(),
    )
    .unwrap();
    let hop = send(&executor, &get(format!("https://{signed_by_b}/"))).await;
    assert_eq!(response(&hop).status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn insecure_reaches_any_certificate_for_any_name() {
    let nobody_trusts = TestCa::generate();
    let addr = spawn_tls(
        echo,
        H2_AND_H1,
        None,
        nobody_trusts.leaf_for_name("other.test"),
    )
    .await;
    let mut settings = settings(HttpVersion::Auto, MIB);
    settings.tls.insecure = true;
    let executor = Executor::with_roots(
        &settings,
        &ResourceLimits::default(),
        Path::new(NO_DIR),
        RootCertStore::empty(),
    )
    .unwrap();
    let hop = send(&executor, &get(format!("https://{addr}/"))).await;
    assert_eq!(response(&hop).status, 200);
}

#[test]
fn insecure_never_reads_the_ca_file() {
    let dir = TempDir::new();
    let mut settings = with_ca_file("missing.pem");
    settings.tls.insecure = true;
    for built in [
        Executor::new(&settings, &ResourceLimits::default(), dir.path()),
        Executor::with_roots(
            &settings,
            &ResourceLimits::default(),
            dir.path(),
            RootCertStore::empty(),
        ),
    ] {
        if let Err(error) = built {
            panic!("an insecure executor should build, but got {error:?}");
        }
    }
}

#[test]
fn ca_file_problems_fail_the_build_without_the_path() {
    let dir = TempDir::new();
    let settings = with_ca_file("missing.pem");
    let dir_text = dir.path().to_str().unwrap();
    for built in [
        Executor::new(&settings, &ResourceLimits::default(), dir.path()),
        Executor::with_roots(
            &settings,
            &ResourceLimits::default(),
            dir.path(),
            RootCertStore::empty(),
        ),
    ] {
        let error = built.err().expect("the ca_file is missing");
        assert!(
            matches!(
                error,
                BuildError::CaFile {
                    at: Scope::File,
                    problem: CaFileProblem::NotFound
                }
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "`http.tls.ca_file` can't be used: the file doesn't exist"
        );
        let shown = format!("{error} {error:?}");
        for secret in ["missing", dir_text] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }
}
