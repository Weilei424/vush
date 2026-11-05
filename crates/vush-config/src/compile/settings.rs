//! The `http` and `limits` blocks.

use vush_core::plan::{
    ByteSize, HttpSettings, HttpVersion, PoolScope, Rate, ResourceLimits, parse_duration,
};

use super::reader::{MapReader, boolean, child, integer, one_of, parsed, string};
use crate::error::Errors;
use crate::node::Node;

const U32_MAX: u64 = u32::MAX as u64;

/// Reads an `http` block over `base`. A key that fails keeps its starting value.
pub(super) fn http(
    node: &Node,
    path: &str,
    base: &HttpSettings,
    errors: &mut Errors,
) -> HttpSettings {
    let mut out = base.clone();
    let Some(mut map) = MapReader::new(node, path, errors) else {
        return out;
    };
    if let Some(n) = map.optional("version") {
        let choices = [
            ("auto", HttpVersion::Auto),
            ("http1", HttpVersion::Http1),
            ("http2", HttpVersion::Http2),
        ];
        if let Some(v) = one_of(n, &child(path, "version"), &choices, errors) {
            out.version = v;
        }
    }
    if let Some(n) = map.optional("pool") {
        let choices = [
            ("shared", PoolScope::Shared),
            ("scenario", PoolScope::Scenario),
            ("vu", PoolScope::Vu),
        ];
        if let Some(v) = one_of(n, &child(path, "pool"), &choices, errors) {
            out.pool = v;
        }
    }
    if let Some(n) = map.optional("timeout")
        && let Some(v) = parsed(n, &child(path, "timeout"), parse_duration, errors)
    {
        out.timeout = v;
    }
    if let Some(n) = map.optional("redirects")
        && let Some(v) = integer(n, &child(path, "redirects"), 0, U32_MAX, errors)
    {
        out.redirects = v as u32;
    }
    if let Some(n) = map.optional("cookies")
        && let Some(v) = boolean(n, &child(path, "cookies"), errors)
    {
        out.cookies = v;
    }
    if let Some(n) = map.optional("max_response_size")
        && let Some(v) = parsed(
            n,
            &child(path, "max_response_size"),
            str::parse::<ByteSize>,
            errors,
        )
    {
        out.max_response_size = v;
    }
    if let Some(n) = map.optional("tls") {
        let tls_path = child(path, "tls");
        if let Some(mut tls) = MapReader::new(n, &tls_path, errors) {
            if let Some(n) = tls.optional("ca_file")
                && let Some(v) = string(n, &child(&tls_path, "ca_file"), errors)
            {
                out.tls.ca_file = Some(v.to_owned());
            }
            if let Some(n) = tls.optional("insecure")
                && let Some(v) = boolean(n, &child(&tls_path, "insecure"), errors)
            {
                out.tls.insecure = v;
            }
            tls.finish(errors);
        }
    }
    map.finish(errors);
    out
}

/// Reads a `limits` block over `base`. The rate is returned only when the
/// block sets it, because it is never inherited.
pub(super) fn limits(
    node: &Node,
    path: &str,
    base: &ResourceLimits,
    errors: &mut Errors,
) -> (ResourceLimits, Option<Rate>) {
    let mut out = base.clone();
    let mut rate = None;
    let Some(mut map) = MapReader::new(node, path, errors) else {
        return (out, rate);
    };
    if let Some(n) = map.optional("max_request_rate") {
        rate = parsed(
            n,
            &child(path, "max_request_rate"),
            str::parse::<Rate>,
            errors,
        );
    }
    if let Some(n) = map.optional("max_variables")
        && let Some(v) = integer(n, &child(path, "max_variables"), 0, U32_MAX, errors)
    {
        out.max_variables = v as u32;
    }
    if let Some(n) = map.optional("max_variable_size")
        && let Some(v) = parsed(
            n,
            &child(path, "max_variable_size"),
            str::parse::<ByteSize>,
            errors,
        )
    {
        out.max_variable_size = v;
    }
    if let Some(n) = map.optional("max_cookies")
        && let Some(v) = integer(n, &child(path, "max_cookies"), 0, U32_MAX, errors)
    {
        out.max_cookies = v as u32;
    }
    if let Some(n) = map.optional("hook_timeout")
        && let Some(v) = parsed(n, &child(path, "hook_timeout"), parse_duration, errors)
    {
        out.hook_timeout = v;
    }
    map.finish(errors);
    (out, rate)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use vush_core::plan::{RateUnit, TlsSettings};

    use super::*;
    use crate::compile::testing::{assert_fails, run};

    #[test]
    fn http_block_sets_every_key() {
        let yaml = "{version: http2, pool: vu, timeout: 10s, redirects: 0, cookies: false, \
                    max_response_size: 2MiB, tls: {ca_file: a.pem, insecure: true}}";
        let (got, errors) = run(yaml, |n, e| http(n, "http", &HttpSettings::default(), e));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            got,
            HttpSettings {
                version: HttpVersion::Http2,
                pool: PoolScope::Vu,
                timeout: Duration::from_secs(10),
                redirects: 0,
                cookies: false,
                max_response_size: ByteSize(2 * 1024 * 1024),
                tls: TlsSettings {
                    ca_file: Some("a.pem".to_owned()),
                    insecure: true
                },
            }
        );
    }

    #[test]
    fn http_block_keeps_the_base_for_missing_keys() {
        let base = HttpSettings {
            timeout: Duration::from_secs(10),
            tls: TlsSettings {
                ca_file: Some("a.pem".to_owned()),
                insecure: false,
            },
            ..HttpSettings::default()
        };
        let (got, errors) = run("{tls: {insecure: true}}", |n, e| http(n, "http", &base, e));
        assert!(errors.is_empty());
        assert_eq!(got.timeout, Duration::from_secs(10));
        assert_eq!(got.tls.ca_file.as_deref(), Some("a.pem"));
        assert!(got.tls.insecure);
    }

    #[test]
    fn a_failed_key_keeps_its_starting_value() {
        let (got, errors) = run("{timeout: banana, redirects: 1}", |n, e| {
            http(n, "http", &HttpSettings::default(), e)
        });
        assert_eq!(got.timeout, Duration::from_secs(30));
        assert_eq!(got.redirects, 1);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn limits_block_sets_every_key() {
        let yaml = "{max_request_rate: 100/s, max_variables: 64, max_variable_size: 16KiB, \
                    max_cookies: 32, hook_timeout: 30s}";
        let (got, errors) = run(yaml, |n, e| {
            limits(n, "limits", &ResourceLimits::default(), e)
        });
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            got.0,
            ResourceLimits {
                max_variables: 64,
                max_variable_size: ByteSize(16 * 1024),
                max_cookies: 32,
                hook_timeout: Duration::from_secs(30),
            }
        );
        assert_eq!(
            got.1,
            Some(Rate {
                count: 100,
                per: RateUnit::Second
            })
        );
    }

    #[test]
    fn limits_keep_the_base_but_never_inherit_the_rate() {
        let base = ResourceLimits {
            max_variables: 128,
            ..ResourceLimits::default()
        };
        let (got, errors) = run("{max_cookies: 32}", |n, e| limits(n, "limits", &base, e));
        assert!(errors.is_empty());
        assert_eq!(got.0.max_variables, 128);
        assert_eq!(got.0.max_cookies, 32);
        assert_eq!(got.1, None);
    }

    #[test]
    fn http_failures_name_their_path() {
        let rows = [
            ("5", "http", "expected a map, found a number"),
            (
                "{version: http3}",
                "http.version",
                "expected one of auto, http1 or http2",
            ),
            (
                "{pool: global}",
                "http.pool",
                "expected one of shared, scenario or vu",
            ),
            ("{timeout: 5}", "http.timeout", "expected a string"),
            ("{timeout: soon}", "http.timeout", "expected a duration"),
            (
                "{redirects: -1}",
                "http.redirects",
                "expected an integer from 0 to 4294967295",
            ),
            (
                "{redirects: 1.5}",
                "http.redirects",
                "expected an integer from 0 to 4294967295",
            ),
            ("{cookies: yes}", "http.cookies", "expected a boolean"),
            (
                "{max_response_size: 1024}",
                "http.max_response_size",
                "expected a string",
            ),
            (
                "{max_response_size: big}",
                "http.max_response_size",
                "expected a size",
            ),
            ("{tls: 5}", "http.tls", "expected a map"),
            (
                "{tls: {ca_file: 5}}",
                "http.tls.ca_file",
                "expected a string",
            ),
            (
                "{tls: {insecure: 1}}",
                "http.tls.insecure",
                "expected a boolean",
            ),
            (
                "{tls: {verify: true}}",
                "http.tls.verify",
                "unknown key `verify`",
            ),
            ("{retries: 1}", "http.retries", "unknown key `retries`"),
        ];
        for (yaml, path, fragment) in rows {
            assert_fails(yaml, path, fragment, |n, e| {
                http(n, "http", &HttpSettings::default(), e)
            });
        }
    }

    #[test]
    fn limits_failures_name_their_path() {
        let rows = [
            (
                "{max_request_rate: 5}",
                "limits.max_request_rate",
                "expected a string",
            ),
            (
                "{max_request_rate: fast}",
                "limits.max_request_rate",
                "expected a rate",
            ),
            (
                "{max_variables: -1}",
                "limits.max_variables",
                "expected an integer from 0 to 4294967295",
            ),
            (
                "{max_variable_size: big}",
                "limits.max_variable_size",
                "expected a size",
            ),
            (
                "{max_cookies: x}",
                "limits.max_cookies",
                "expected an integer from 0 to 4294967295",
            ),
            (
                "{hook_timeout: x}",
                "limits.hook_timeout",
                "expected a duration",
            ),
            ("{nope: 1}", "limits.nope", "unknown key `nope`"),
        ];
        for (yaml, path, fragment) in rows {
            assert_fails(yaml, path, fragment, |n, e| {
                limits(n, "limits", &ResourceLimits::default(), e)
            });
        }
    }
}
