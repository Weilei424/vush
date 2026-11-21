//! Every fixed text that becomes a `Failure::message`.
//!
//! A failure's message never holds a URL, header value, body byte, cookie or
//! library text. Each one is a constant here, and `ALL` lists them for the
//! catalogue test.

// `invalid_request`, from `prepare` and the connector.
pub(crate) const URL_UNPARSABLE: &str = "the URL can't be parsed";
pub(crate) const BASE_URL_QUERY_OR_FRAGMENT: &str = "the base_url has a query or a fragment";
pub(crate) const URL_UNSUPPORTED_SCHEME: &str = "only http and https URLs are supported";
pub(crate) const URL_NO_HOST: &str = "the URL has no host";
pub(crate) const URL_CREDENTIALS: &str = "the URL holds a user name or password";
pub(crate) const CONNECT_METHOD: &str = "CONNECT isn't supported";
pub(crate) const METHOD_INVALID: &str = "the method is invalid";
pub(crate) const HEADER_NAME_INVALID: &str = "a header name is invalid";
pub(crate) const HEADER_FORBIDDEN: &str = "this header can't be set by a request";
pub(crate) const HEADER_VALUE_CONTROL: &str = "a header value holds a control character";
pub(crate) const TOO_MANY_HEADERS: &str = "the request has too many headers";
pub(crate) const FOREIGN_SESSION: &str = "the session belongs to another executor";

// `invalid_redirect`, from `follow`.
pub(crate) const REDIRECT_MANY_LOCATIONS: &str = "the redirect has more than one Location";
pub(crate) const REDIRECT_UNPARSABLE: &str = "the redirect's Location can't be parsed";
pub(crate) const REDIRECT_NO_HOST: &str = "the redirect's URL has no host";
pub(crate) const REDIRECT_UNSUPPORTED_SCHEME: &str = "the redirect goes to an unsupported scheme";
pub(crate) const REDIRECT_CREDENTIALS: &str = "the redirect's URL holds a user name or password";

// `dns`, `connect`, `tls` and `timeout`, from the connector and `classify`.
pub(crate) const HOST_UNRESOLVED: &str = "the host name didn't resolve";
pub(crate) const CONNECTION_UNREACHABLE: &str = "the connection was refused or unreachable";
pub(crate) const HANDSHAKE_FAILED: &str = "the TLS handshake failed";
pub(crate) const NO_HTTP2: &str = "the server didn't agree to HTTP/2";
pub(crate) const CONNECTION_TOOK_TOO_LONG: &str = "the connection took too long";
pub(crate) const SHARED_CONNECT_FAILED: &str = "the shared connection attempt failed";

// The rest, from `classify`.
pub(crate) const HOP_TIMED_OUT: &str = "the hop ran out of time";
pub(crate) const CONNECTION_CLOSED: &str = "the connection closed before the response was complete";
pub(crate) const PROTOCOL_BROKEN: &str = "the response broke HTTP rules";
pub(crate) const HEAD_TOO_LARGE: &str = "the response head is too large";
pub(crate) const UNCLASSIFIED: &str = "the request failed for an unclassified reason";

/// Every text above, once each. Only the catalogue test reads it.
#[cfg(test)]
pub(crate) const ALL: &[&str] = &[
    URL_UNPARSABLE,
    BASE_URL_QUERY_OR_FRAGMENT,
    URL_UNSUPPORTED_SCHEME,
    URL_NO_HOST,
    URL_CREDENTIALS,
    CONNECT_METHOD,
    METHOD_INVALID,
    HEADER_NAME_INVALID,
    HEADER_FORBIDDEN,
    HEADER_VALUE_CONTROL,
    TOO_MANY_HEADERS,
    FOREIGN_SESSION,
    REDIRECT_MANY_LOCATIONS,
    REDIRECT_UNPARSABLE,
    REDIRECT_NO_HOST,
    REDIRECT_UNSUPPORTED_SCHEME,
    REDIRECT_CREDENTIALS,
    HOST_UNRESOLVED,
    CONNECTION_UNREACHABLE,
    HANDSHAKE_FAILED,
    NO_HTTP2,
    CONNECTION_TOOK_TOO_LONG,
    SHARED_CONNECT_FAILED,
    HOP_TIMED_OUT,
    CONNECTION_CLOSED,
    PROTOCOL_BROKEN,
    HEAD_TOO_LARGE,
    UNCLASSIFIED,
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::ALL;

    #[test]
    fn every_text_is_short_fixed_and_unique() {
        let mut seen = HashSet::new();
        for text in ALL {
            assert!(!text.is_empty(), "empty text");
            assert!(text.is_ascii(), "{text}");
            assert!(text.len() <= 80, "{text}");
            assert!(!text.contains(['{', '}']), "{text}");
            assert!(seen.insert(*text), "duplicate: {text}");
        }
        assert_eq!(ALL.len(), 28);
    }
}
