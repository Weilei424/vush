//! HTTP settings and resource limits, resolved and with the schema defaults.

use std::time::Duration;

use super::value::ByteSize;

/// Which HTTP version a connection may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpVersion {
    /// Negotiate the version with the server.
    Auto,
    Http1,
    Http2,
}

/// How widely an HTTP connection pool is shared.
///
/// Sharing a pool never shares cookies or credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolScope {
    /// One pool for the whole run.
    Shared,
    /// One pool per scenario.
    Scenario,
    /// One pool per virtual user.
    Vu,
}

/// TLS settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsSettings {
    /// A CA bundle path, stored as text and read at run start.
    pub ca_file: Option<String>,
    /// Skip certificate verification.
    pub insecure: bool,
}

/// HTTP settings, after file-level and scenario-level values are merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpSettings {
    pub version: HttpVersion,
    pub pool: PoolScope,
    /// The timeout of one physical attempt.
    pub timeout: Duration,
    /// How many redirects to follow. `0` means none.
    pub redirects: u32,
    /// Whether each user keeps cookies.
    pub cookies: bool,
    pub max_response_size: ByteSize,
    pub tls: TlsSettings,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            version: HttpVersion::Auto,
            pool: PoolScope::Shared,
            timeout: Duration::from_secs(30),
            redirects: 5,
            cookies: true,
            max_response_size: ByteSize(1_048_576),
            tls: TlsSettings {
                ca_file: None,
                insecure: false,
            },
        }
    }
}

/// Per-user and per-hook resource limits, after merging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLimits {
    /// The most variables one user may hold.
    pub max_variables: u32,
    /// The largest size of one variable value.
    pub max_variable_size: ByteSize,
    /// The most cookies one user may hold.
    pub max_cookies: u32,
    /// The timeout of one whole hook run.
    pub hook_timeout: Duration,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_variables: 256,
            max_variable_size: ByteSize(65_536),
            max_cookies: 64,
            hook_timeout: Duration::from_secs(60),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_settings_default_matches_schema() {
        assert_eq!(
            HttpSettings::default(),
            HttpSettings {
                version: HttpVersion::Auto,
                pool: PoolScope::Shared,
                timeout: Duration::from_secs(30),
                redirects: 5,
                cookies: true,
                max_response_size: ByteSize(1_048_576),
                tls: TlsSettings {
                    ca_file: None,
                    insecure: false
                },
            }
        );
    }

    #[test]
    fn resource_limits_default_matches_schema() {
        assert_eq!(
            ResourceLimits::default(),
            ResourceLimits {
                max_variables: 256,
                max_variable_size: ByteSize(65_536),
                max_cookies: 64,
                hook_timeout: Duration::from_secs(60),
            }
        );
    }
}
