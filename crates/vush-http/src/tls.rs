//! The client's TLS settings: the `ring` provider, the given roots, and an
//! ALPN offer that follows the version mode.

use std::sync::Arc;

use rustls::{ClientConfig, RootCertStore};
use vush_core::plan::HttpVersion;

/// A client config that trusts `roots` and offers the protocols `version`
/// allows, in order of preference.
pub(crate) fn client_config(roots: RootCertStore, version: HttpVersion) -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = match version {
        HttpVersion::Auto => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        HttpVersion::Http1 => vec![b"http/1.1".to_vec()],
        HttpVersion::Http2 => vec![b"h2".to_vec()],
    };
    Arc::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_offers_follow_the_version_mode() {
        let cases: [(HttpVersion, &[&[u8]]); 3] = [
            (HttpVersion::Auto, &[b"h2", b"http/1.1"]),
            (HttpVersion::Http1, &[b"http/1.1"]),
            (HttpVersion::Http2, &[b"h2"]),
        ];
        for (version, offer) in cases {
            let config = client_config(RootCertStore::empty(), version);
            assert_eq!(config.alpn_protocols, offer, "{version:?}");
        }
    }
}
