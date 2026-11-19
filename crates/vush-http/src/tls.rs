//! The client's TLS settings: the `ring` provider, whom to trust, and an
//! ALPN offer that follows the version mode.
//!
//! Trust is the system's roots, the system's roots plus the certificates in
//! a `tls.ca_file`, or, under `tls.insecure`, any certificate for any name.
//! Under `pool: vu`, each user gets a copy of the config with a TLS session
//! cache of its own.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

use rustls::client::Resumption;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use vush_core::plan::{HttpSettings, HttpVersion};

use crate::executor::CaFileProblem;

/// The largest `tls.ca_file` that is read: 1 MiB.
pub(crate) const MAX_CA_FILE_BYTES: u64 = 1_048_576;

/// The servers one user's TLS session cache remembers. A user talks to few
/// origins, and the cache is preallocated per user, so it stays small.
pub(crate) const USER_SESSION_CACHE: usize = 8;

/// The sessions rustls keeps for each server it remembers: up to 8 TLS 1.3
/// tickets. Its cache is sized in sessions, not servers.
const SESSIONS_PER_SERVER: usize = 8;

/// Whom a scope's connections trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Trust {
    /// The system's roots.
    System,
    /// The system's roots plus the certificates in the file at this path.
    /// The path is kept as text, so two spellings of one file are two
    /// trusts, even those that `Path` compares as equal, such as `ca.pem`
    /// and `ca.pem/`. Each spelling is read on its own.
    SystemPlus(OsString),
    /// Any certificate, for any name.
    Insecure,
}

impl Trust {
    /// The trust `settings` ask for. `insecure` wins over `ca_file`, which
    /// is resolved against `test_dir`, so an absolute path is used as
    /// written.
    pub(crate) fn of(settings: &HttpSettings, test_dir: &Path) -> Self {
        if settings.tls.insecure {
            return Self::Insecure;
        }
        match &settings.tls.ca_file {
            Some(ca_file) => Self::SystemPlus(test_dir.join(ca_file).into_os_string()),
            None => Self::System,
        }
    }
}

/// How a client checks the server's certificate.
pub(crate) enum Verification {
    /// It must chain to one of these roots and name the URL's host.
    Roots(RootCertStore),
    /// It isn't checked, but the handshake's signatures still are.
    Insecure,
}

/// A client config that verifies as `verification` says and offers the
/// protocols `version` allows, in order of preference.
pub(crate) fn client_config(verification: Verification, version: HttpVersion) -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let algorithms = provider.signature_verification_algorithms;
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions");
    let builder = match verification {
        Verification::Roots(roots) => builder.with_root_certificates(roots),
        Verification::Insecure => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate { algorithms })),
    };
    let mut config = builder.with_no_client_auth();
    config.alpn_protocols = match version {
        HttpVersion::Auto => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        HttpVersion::Http1 => vec![b"http/1.1".to_vec()],
        HttpVersion::Http2 => vec![b"h2".to_vec()],
    };
    Arc::new(config)
}

/// A copy of `scope`, a `pool: vu` scope's config, with a TLS session cache
/// of its own that remembers [`USER_SESSION_CACHE`] servers. Each user gets
/// one, so users never resume each other's sessions.
pub(crate) fn user_config(scope: &ClientConfig) -> Arc<ClientConfig> {
    let mut config = scope.clone();
    config.resumption = Resumption::in_memory_sessions(USER_SESSION_CACHE * SESSIONS_PER_SERVER);
    Arc::new(config)
}

/// The system's root certificates. Those that don't parse are skipped,
/// because nobody chose them.
pub(crate) fn system_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    roots
}

/// Adds every certificate in the PEM file at `path` to `roots`. Sections
/// other than certificates, such as a key, are skipped. The path must name a
/// regular file, which must be at most [`MAX_CA_FILE_BYTES`] long and hold
/// at least one certificate, and each of its certificates must be usable as
/// a root. `roots` changes only when the whole file is.
pub(crate) fn add_ca_file(roots: &mut RootCertStore, path: &Path) -> Result<(), CaFileProblem> {
    let pem = read_ca_file(path)?;
    let mut added = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(&pem) {
        let cert = cert.map_err(|_| CaFileProblem::BadCertificate)?;
        added.add(cert).map_err(|_| CaFileProblem::BadCertificate)?;
    }
    if added.is_empty() {
        return Err(CaFileProblem::NoCertificates);
    }
    roots.extend(added.roots);
    Ok(())
}

/// The file's bytes. No more than one byte past the bound is read. A path
/// that isn't a regular file, such as a directory or a named pipe, is
/// `Unreadable` and never opened, so it can't block the build.
fn read_ca_file(path: &Path) -> Result<Vec<u8>, CaFileProblem> {
    if !std::fs::metadata(path).map_err(problem)?.is_file() {
        return Err(CaFileProblem::Unreadable);
    }
    let mut pem = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_CA_FILE_BYTES + 1).read_to_end(&mut pem))
        .map_err(problem)?;
    if pem.len() as u64 > MAX_CA_FILE_BYTES {
        return Err(CaFileProblem::TooLarge);
    }
    Ok(pem)
}

/// A missing file is `NotFound`. Any other error is `Unreadable`.
fn problem(error: io::Error) -> CaFileProblem {
    match error.kind() {
        io::ErrorKind::NotFound => CaFileProblem::NotFound,
        _ => CaFileProblem::Unreadable,
    }
}

/// The verifier under `tls.insecure`. It accepts any certificate chain for
/// any name, but still checks the handshake's signatures with the
/// provider's algorithms, so the connection is still TLS, just
/// unauthenticated.
#[derive(Debug)]
struct AcceptAnyCertificate {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair};

    use super::*;

    const SCHEMA: &str = include_str!("../../../docs/config-schema.md");

    /// A directory of its own under the system's temp dir, named from the
    /// process ID and a counter, and removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = format!("vush-http-unit-{}-{n}", std::process::id());
            let path = std::env::temp_dir().join(name);
            // A directory left by a process that had the same ID.
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ca() -> Certificate {
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "vush test ca");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.self_signed(&KeyPair::generate().unwrap()).unwrap()
    }

    /// A store that holds one CA, so a test can tell whether a file added to
    /// it.
    fn one_root() -> RootCertStore {
        let mut roots = RootCertStore::empty();
        roots.add(ca().der().clone()).unwrap();
        roots
    }

    fn certificate_section(body: &str) -> String {
        format!("-----BEGIN CERTIFICATE-----\n{body}\n-----END CERTIFICATE-----\n")
    }

    /// `pem`, with `\n` added until it is `len` bytes long.
    fn padded(pem: &str, len: u64) -> Vec<u8> {
        let mut bytes = pem.as_bytes().to_vec();
        bytes.resize(usize::try_from(len).unwrap(), b'\n');
        bytes
    }

    #[test]
    fn alpn_offers_follow_the_version_mode() {
        let cases: [(HttpVersion, &[&[u8]]); 3] = [
            (HttpVersion::Auto, &[b"h2", b"http/1.1"]),
            (HttpVersion::Http1, &[b"http/1.1"]),
            (HttpVersion::Http2, &[b"h2"]),
        ];
        for (version, offer) in cases {
            for (verifies, verification) in [
                ("roots", Verification::Roots(RootCertStore::empty())),
                ("insecure", Verification::Insecure),
            ] {
                let config = client_config(verification, version);
                assert_eq!(config.alpn_protocols, offer, "{version:?} with {verifies}");
            }
        }
    }

    #[test]
    fn ca_file_problems_are_told_apart() {
        let dir = TempDir::new();
        let key = KeyPair::generate().unwrap().serialize_pem();
        // `bm90IGEgY2VydGlmaWNhdGU=` is `not a certificate` in base64.
        let not_a_certificate = certificate_section("bm90IGEgY2VydGlmaWNhdGU=");
        let good_then_bad = format!("{}{}", ca().pem(), certificate_section("!!!"));
        let too_large = vec![b'x'; usize::try_from(MAX_CA_FILE_BYTES + 1).unwrap()];
        let cases = [
            (dir.0.join("missing.pem"), CaFileProblem::NotFound),
            (dir.0.clone(), CaFileProblem::Unreadable),
            (dir.write("empty.pem", b""), CaFileProblem::NoCertificates),
            (
                dir.write("key.pem", key.as_bytes()),
                CaFileProblem::NoCertificates,
            ),
            (
                dir.write("hello.pem", b"hello"),
                CaFileProblem::NoCertificates,
            ),
            (
                dir.write("bad.pem", certificate_section("!!!").as_bytes()),
                CaFileProblem::BadCertificate,
            ),
            (
                dir.write("good-then-bad.pem", good_then_bad.as_bytes()),
                CaFileProblem::BadCertificate,
            ),
            (
                dir.write("fake.pem", not_a_certificate.as_bytes()),
                CaFileProblem::BadCertificate,
            ),
            (dir.write("large.pem", &too_large), CaFileProblem::TooLarge),
        ];
        for (path, problem) in cases {
            let mut roots = one_root();
            assert_eq!(add_ca_file(&mut roots, &path), Err(problem), "{path:?}");
            assert_eq!(roots.len(), 1, "{path:?} changed the store");
        }
    }

    #[test]
    fn every_certificate_in_a_file_is_added() {
        let dir = TempDir::new();
        let both = format!("{}{}", ca().pem(), ca().pem());
        let path = dir.write("both.pem", both.as_bytes());
        let mut roots = one_root();
        assert_eq!(add_ca_file(&mut roots, &path), Ok(()));
        assert_eq!(roots.len(), 3);
    }

    #[test]
    fn key_sections_are_skipped() {
        let dir = TempDir::new();
        let key = KeyPair::generate().unwrap().serialize_pem();
        let path = dir.write("key-then-ca.pem", format!("{key}{}", ca().pem()).as_bytes());
        let mut roots = RootCertStore::empty();
        assert_eq!(add_ca_file(&mut roots, &path), Ok(()));
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn crlf_pem_files_load() {
        let dir = TempDir::new();
        let crlf = ca().pem().replace('\n', "\r\n");
        let path = dir.write("crlf.pem", crlf.as_bytes());
        let mut roots = RootCertStore::empty();
        assert_eq!(add_ca_file(&mut roots, &path), Ok(()));
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn the_bound_is_one_mib() {
        let dir = TempDir::new();
        let pem = ca().pem();
        let at_the_bound = dir.write("at.pem", &padded(&pem, MAX_CA_FILE_BYTES));
        let past_the_bound = dir.write("past.pem", &padded(&pem, MAX_CA_FILE_BYTES + 1));
        let mut roots = RootCertStore::empty();
        assert_eq!(add_ca_file(&mut roots, &at_the_bound), Ok(()));
        assert_eq!(roots.len(), 1);
        assert_eq!(
            add_ca_file(&mut roots, &past_the_bound),
            Err(CaFileProblem::TooLarge)
        );
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn trust_follows_the_settings() {
        let test_dir = Path::new("/t");
        let mut settings = HttpSettings::default();
        assert_eq!(Trust::of(&settings, test_dir), Trust::System);
        settings.tls.ca_file = Some("ca.pem".to_owned());
        assert_eq!(
            Trust::of(&settings, test_dir),
            Trust::SystemPlus("/t/ca.pem".into())
        );
        settings.tls.insecure = true;
        assert_eq!(Trust::of(&settings, test_dir), Trust::Insecure);
        settings.tls.ca_file = None;
        assert_eq!(Trust::of(&settings, test_dir), Trust::Insecure);
    }

    #[test]
    fn the_schema_states_the_ca_file_bound() {
        let row = SCHEMA
            .lines()
            .find(|line| line.starts_with("| `tls.ca_file` |"))
            .expect("a `tls.ca_file` row in the schema");
        assert!(row.contains("At most 1 MiB"), "{row}");
        assert_eq!(MAX_CA_FILE_BYTES, 1024 * 1024);
    }
}
