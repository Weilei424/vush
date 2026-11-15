//! Checking and encoding a request before it is sent.

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Uri};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use url::Url;
use vush_core::protocol::http::{HttpBody, HttpRequest, RequestUrl};
use vush_core::protocol::{Failure, FailureKind, FailureStage};

/// The `User-Agent` sent unless the request sets its own.
pub(crate) const USER_AGENT: &str = concat!("vush/", env!("CARGO_PKG_VERSION"));

/// Header names a request can't set, in lowercase. The executor owns framing.
pub(crate) const FORBIDDEN: [&str; 7] = [
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "upgrade",
    "te",
];

/// Every byte except `A-Z a-z 0-9 - . _ ~` is percent-encoded in a query.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// A request that passed every check, ready for the wire.
pub(crate) struct Prepared {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
}

fn invalid(message: &'static str) -> Failure {
    Failure {
        kind: FailureKind::InvalidRequest,
        stage: FailureStage::Prepare,
        message,
    }
}

/// The most header names a request holds of its own, until P2.5 sets the
/// real limit. Repeats of one name count once, and the `Content-Type` and
/// `User-Agent` that `prepare` adds don't count.
///
/// hyper and hyper-util add `Host` and `Content-Length` with calls that
/// panic if `http::HeaderMap` would have to grow past its largest size,
/// 32,768 slots. Names that collide in its hash make it grow early, but past
/// that size only once it holds 6,554 names, a fifth of its slots. This
/// limit keeps every request far below that.
pub(crate) const MAX_HEADER_NAMES: usize = 4096;

fn too_many() -> Failure {
    invalid("the request has too many headers")
}

fn parse(text: &str) -> Result<Url, Failure> {
    Url::parse(text).map_err(|_| invalid("the URL can't be parsed"))
}

/// The URL that is sent: joined or parsed, checked, without its fragment, and
/// with the query pairs appended.
pub(crate) fn request_url(request: &HttpRequest) -> Result<Url, Failure> {
    let mut url = match &request.url {
        RequestUrl::Absolute(text) => parse(text)?,
        RequestUrl::Joined { base, path } => {
            let base = parse(base)?;
            if base.query().is_some() || base.fragment().is_some() {
                return Err(invalid("the base_url has a query or a fragment"));
            }
            if path.is_empty() {
                base
            } else {
                let joined = format!(
                    "{}/{}",
                    base.as_str().trim_end_matches('/'),
                    path.trim_start_matches('/')
                );
                parse(&joined)?
            }
        }
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("only http and https URLs are supported"));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(invalid("the URL has no host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("the URL holds a user name or password"));
    }
    url.set_fragment(None);
    if !request.query.is_empty() {
        let mut query = url.query().unwrap_or("").to_owned();
        for (name, value) in &request.query {
            if !query.is_empty() {
                query.push('&');
            }
            query.extend(utf8_percent_encode(name, QUERY));
            query.push('=');
            query.extend(utf8_percent_encode(value, QUERY));
        }
        url.set_query(Some(&query));
    }
    Ok(url)
}

/// Checks `request` and encodes it. Nothing is sent.
pub(crate) fn prepare(request: &HttpRequest) -> Result<Prepared, Failure> {
    let url = request_url(request)?;
    if request.method.as_str() == "CONNECT" {
        return Err(invalid("CONNECT isn't supported"));
    }
    let method = Method::from_bytes(request.method.as_str().as_bytes())
        .map_err(|_| invalid("the method is invalid"))?;

    let mut headers = HeaderMap::new();
    for (name, value) in &request.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| invalid("a header name is invalid"))?;
        if FORBIDDEN.contains(&name.as_str()) {
            return Err(invalid("this header can't be set by a request"));
        }
        // `from_bytes` refuses 0x00-0x1F except tab, and 0x7F.
        let value = HeaderValue::from_bytes(value.as_bytes())
            .map_err(|_| invalid("a header value holds a control character"))?;
        if headers.keys_len() == MAX_HEADER_NAMES && !headers.contains_key(&name) {
            return Err(too_many());
        }
        headers.try_append(name, value).map_err(|_| too_many())?;
    }

    let (body, content_type) = match &request.body {
        None => (Vec::new(), None),
        Some(HttpBody::Json(value)) => {
            let mut out = Vec::new();
            crate::json::encode(value, &mut out);
            (out, Some("application/json"))
        }
        Some(HttpBody::Text(text)) => (text.as_bytes().to_vec(), Some("text/plain; charset=utf-8")),
        Some(HttpBody::Form(fields)) => (
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(fields)
                .finish()
                .into_bytes(),
            Some("application/x-www-form-urlencoded"),
        ),
    };
    if let Some(content_type) = content_type
        && !headers.contains_key(http::header::CONTENT_TYPE)
    {
        headers
            .try_insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static(content_type),
            )
            .map_err(|_| too_many())?;
    }
    if !headers.contains_key(http::header::USER_AGENT) {
        headers
            .try_insert(
                http::header::USER_AGENT,
                HeaderValue::from_static(USER_AGENT),
            )
            .map_err(|_| too_many())?;
    }

    let uri = url
        .as_str()
        .parse()
        .map_err(|_| invalid("the URL can't be parsed"))?;
    Ok(Prepared {
        method,
        uri,
        headers,
        body: Bytes::from(body),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use vush_core::plan::JsonNumber;
    use vush_core::protocol::JsonValue;

    const TOKEN: &str =
        "!#$%&'*+-.^_`|~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    fn request(url: RequestUrl) -> HttpRequest {
        HttpRequest {
            method: "GET".parse().unwrap(),
            url,
            query: vec![],
            headers: vec![],
            body: None,
        }
    }

    fn absolute(url: &str) -> HttpRequest {
        request(RequestUrl::Absolute(url.to_owned()))
    }

    fn with_headers(items: &[(&str, &str)]) -> HttpRequest {
        let mut r = absolute("https://h/");
        r.headers = pairs(items);
        r
    }

    fn invalid(message: &'static str) -> Failure {
        Failure {
            kind: FailureKind::InvalidRequest,
            stage: FailureStage::Prepare,
            message,
        }
    }

    fn joined(base: &str, path: &str) -> HttpRequest {
        request(RequestUrl::Joined {
            base: base.to_owned(),
            path: path.to_owned(),
        })
    }

    #[test]
    fn joins_path_to_base_with_one_slash() {
        let cases = [
            ("https://api.test/v1", "/users", "https://api.test/v1/users"),
            ("https://api.test/v1/", "users", "https://api.test/v1/users"),
            (
                "https://api.test",
                "/users?active=1",
                "https://api.test/users?active=1",
            ),
            ("https://api.test/v1", "", "https://api.test/v1"),
            (
                "https://api.test/v1//",
                "//users",
                "https://api.test/v1/users",
            ),
        ];
        for (base, path, want) in cases {
            let url = request_url(&joined(base, path)).unwrap();
            assert_eq!(url.as_str(), want, "{base} + {path}");
        }
    }

    #[test]
    fn a_base_with_a_query_or_fragment_is_invalid() {
        for base in ["https://h/?x", "https://h/#f"] {
            assert_eq!(
                request_url(&joined(base, "/p")).unwrap_err(),
                invalid("the base_url has a query or a fragment"),
                "{base}"
            );
        }
    }

    #[test]
    fn absolute_urls_are_checked() {
        let cases = [
            ("ftp://h/x", Err("only http and https URLs are supported")),
            (
                "https://u:p@h/",
                Err("the URL holds a user name or password"),
            ),
            ("not a url", Err("the URL can't be parsed")),
            ("https://h/x#frag", Ok("https://h/x")),
        ];
        for (url, want) in cases {
            let got = request_url(&absolute(url));
            match want {
                Ok(text) => assert_eq!(got.unwrap().as_str(), text, "{url}"),
                Err(message) => assert_eq!(got.unwrap_err(), invalid(message), "{url}"),
            }
        }
    }

    #[test]
    fn query_pairs_are_percent_encoded_and_appended() {
        let cases = [
            (
                "https://h/p",
                vec![("a b", "c&d=e"), ("é", "+/?#")],
                "https://h/p?a%20b=c%26d%3De&%C3%A9=%2B%2F%3F%23",
            ),
            ("https://h/p?x=1", vec![("y", "2")], "https://h/p?x=1&y=2"),
            ("https://h/p", vec![("k", "a b")], "https://h/p?k=a%20b"),
            ("https://h/p", vec![], "https://h/p"),
            (
                "https://h/p",
                vec![("AZaz09-._~", "AZaz09-._~")],
                "https://h/p?AZaz09-._~=AZaz09-._~",
            ),
        ];
        for (url, query, want) in cases {
            let mut r = absolute(url);
            r.query = pairs(&query);
            assert_eq!(request_url(&r).unwrap().as_str(), want, "{url}");
        }
    }

    #[test]
    fn form_bodies_use_plus_for_spaces() {
        let mut r = absolute("https://h/");
        r.body = Some(HttpBody::Form(pairs(&[("a b", "c+d"), ("x", "é")])));
        assert_eq!(
            prepare(&r).unwrap().body.as_ref(),
            b"a+b=c%2Bd&x=%C3%A9".as_slice()
        );
    }

    #[test]
    fn default_headers_follow_the_body() {
        let bodies = [
            (
                Some(HttpBody::Json(JsonValue::Null)),
                Some("application/json"),
            ),
            (
                Some(HttpBody::Form(vec![])),
                Some("application/x-www-form-urlencoded"),
            ),
            (
                Some(HttpBody::Text(String::new())),
                Some("text/plain; charset=utf-8"),
            ),
            (None, None),
        ];
        for (body, want) in bodies {
            let mut r = absolute("https://h/");
            r.body = body;
            let p = prepare(&r).unwrap();
            let got = p.headers.get("content-type").map(|v| v.to_str().unwrap());
            assert_eq!(got, want);
            assert_eq!(p.headers.get("user-agent").unwrap(), USER_AGENT);
        }

        let mut r = with_headers(&[("content-TYPE", "x"), ("USER-agent", "y")]);
        r.body = Some(HttpBody::Text("t".into()));
        let p = prepare(&r).unwrap();
        assert_eq!(p.headers.get_all("content-type").iter().count(), 1);
        assert_eq!(p.headers.get("content-type").unwrap(), "x");
        assert_eq!(p.headers.get_all("user-agent").iter().count(), 1);
        assert_eq!(p.headers.get("user-agent").unwrap(), "y");
    }

    #[test]
    fn header_names_match_http_tokens() {
        for b in 0u8..=0x7f {
            let in_token = TOKEN.as_bytes().contains(&b);
            assert_eq!(
                http::HeaderName::from_bytes(&[b]).is_ok(),
                in_token,
                "byte {b:#04x}"
            );
        }
    }

    #[test]
    fn bad_headers_are_refused() {
        let name = invalid("a header name is invalid");
        let forbidden = invalid("this header can't be set by a request");
        let control = invalid("a header value holds a control character");
        for f in [
            "Content-Length",
            "TRANSFER-encoding",
            "Connection",
            "keep-ALIVE",
            "Proxy-Connection",
            "Upgrade",
            "Te",
        ] {
            assert_eq!(
                prepare(&with_headers(&[(f, "x")])).err().unwrap(),
                forbidden,
                "{f}"
            );
        }
        assert_eq!(
            prepare(&with_headers(&[("bad name", "x")])).err().unwrap(),
            name
        );
        assert_eq!(
            prepare(&with_headers(&[("x-a", "a\r\nb")])).err().unwrap(),
            control
        );
        for b in (0u8..0x20).filter(|b| *b != b'\t').chain([0x7f]) {
            let value = format!("a{}b", b as char);
            assert_eq!(
                prepare(&with_headers(&[("x-a", &value)])).err().unwrap(),
                control,
                "byte {b:#04x}"
            );
        }
        let p = prepare(&with_headers(&[
            ("x-a", "a\tb"),
            ("x-b", "é"),
            ("Host", "h2"),
            ("Authorization", "t"),
            ("Cookie", "c=1"),
        ]))
        .unwrap();
        assert_eq!(p.headers.get("x-a").unwrap().as_bytes(), b"a\tb");
        assert_eq!(p.headers.get("x-b").unwrap().as_bytes(), "é".as_bytes());
        for n in ["host", "authorization", "cookie"] {
            assert!(p.headers.contains_key(n), "{n}");
        }
    }

    fn with_names<S: AsRef<str>>(names: &[S]) -> HttpRequest {
        let mut r = absolute("https://h/");
        r.headers = names
            .iter()
            .map(|name| (name.as_ref().to_owned(), "v".into()))
            .collect();
        r
    }

    fn distinct_names(count: usize) -> Vec<String> {
        (0..count).map(|n| format!("x-{n}")).collect()
    }

    /// The hash `http::HeaderMap` gives a custom name until it switches to a
    /// keyed hash: FNV-1a over what `HeaderName` hashes (its enum tag, then
    /// its bytes), cut to 15 bits.
    fn map_hash(name: &str) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for &byte in 1isize.to_ne_bytes().iter().chain(name.as_bytes()) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash & 0x7fff
    }

    fn names_hashing_to(prefix: &str, hash: u64, count: usize) -> Vec<String> {
        let mut name = String::new();
        let mut found = Vec::new();
        for n in 0.. {
            name.clear();
            write!(name, "{prefix}-{n}").unwrap();
            if map_hash(&name) == hash {
                found.push(name.clone());
                if found.len() == count {
                    break;
                }
            }
        }
        found
    }

    /// `count` header names, the request's own `User-Agent` among them,
    /// that leave the map in its danger state. 140 names hash to the slot
    /// after `SLOT` and fill one run of slots. Two names hash to `SLOT`: the
    /// first takes it, and the last takes the run's first slot and shifts
    /// the whole run, so the map's next insert must grow it. The other names
    /// keep clear of those slots at every size the map takes.
    fn names_that_shift_a_run(count: usize) -> Vec<String> {
        const SLOT: u64 = 0x1234;
        let clear = |hash: u64| {
            [0x1fff, 0x3fff, 0x7fff]
                .iter()
                .all(|mask| !(SLOT - 256..SLOT + 512).contains(&(hash & mask)))
        };
        let run = names_hashing_to("a", SLOT + 1, 140);
        let shift = names_hashing_to("b", SLOT, 2);
        let mut names = vec!["user-agent".to_owned()];
        names.extend(
            (0..)
                .map(|n| format!("x-{n}"))
                .filter(|name| clear(map_hash(name)))
                .take(count - 1 - run.len() - shift.len()),
        );
        names.extend(run);
        names.extend(shift);
        names
    }

    /// What hyper-util and hyper do to the map after `prepare`, with the
    /// same calls, which panic when the map can't grow.
    fn add_like_hyper(headers: &mut HeaderMap) {
        headers
            .entry(http::header::HOST)
            .or_insert_with(|| HeaderValue::from_static("h"));
        let _ = headers.entry(http::header::TRANSFER_ENCODING);
        headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from(1u64));
        headers
            .entry(http::header::CONTENT_LENGTH)
            .or_insert_with(|| HeaderValue::from(1u64));
    }

    #[test]
    fn a_request_holds_at_most_the_header_name_limit() {
        let too_many = invalid("the request has too many headers");
        let most = MAX_HEADER_NAMES;

        // The content type and the user agent don't count, and neither do
        // repeats of one name.
        let mut r = with_names(&distinct_names(most));
        r.headers.push(("x-0".into(), "again".into()));
        r.body = Some(HttpBody::Text("t".into()));
        let p = prepare(&r).unwrap();
        assert_eq!(p.headers.keys_len(), most + 2);
        assert_eq!(p.headers.len(), most + 3);

        let mut over = distinct_names(most + 1);
        assert_eq!(prepare(&with_names(&over)).err().unwrap(), too_many);
        // The name past the limit is refused even after a repeat.
        over.insert(most, "x-0".into());
        assert_eq!(prepare(&with_names(&over)).err().unwrap(), too_many);
    }

    /// Hyper's calls panic when the map would grow past its largest size.
    /// The map grows early once its names collide, but even then only at
    /// 6,554 names or more, and the limit stays far below that.
    #[test]
    fn colliding_header_names_leave_hyper_room() {
        let mut p = prepare(&with_names(&names_that_shift_a_run(MAX_HEADER_NAMES))).unwrap();
        let before = p.headers.capacity();
        assert!(p.headers.keys_len() < before);
        add_like_hyper(&mut p.headers);
        // Only the danger state grows a map that still had room.
        assert!(p.headers.capacity() > before);
        assert_eq!(p.headers.keys_len(), MAX_HEADER_NAMES + 2);
    }

    #[test]
    fn the_prepared_request_carries_method_and_target() {
        let mut r = absolute("HTTPS://H/a b#f");
        r.method = "PUT".parse().unwrap();
        let p = prepare(&r).unwrap();
        assert_eq!(p.method, Method::PUT);
        assert_eq!(p.uri.to_string(), "https://h/a%20b");
    }

    #[test]
    fn connect_is_refused() {
        let mut r = absolute("https://h/");
        r.method = "CONNECT".parse().unwrap();
        assert_eq!(
            prepare(&r).err().unwrap(),
            invalid("CONNECT isn't supported")
        );
    }

    #[test]
    fn repeated_headers_keep_their_order() {
        let p = prepare(&with_headers(&[("x-a", "1"), ("x-b", "2"), ("x-a", "3")])).unwrap();
        let all: Vec<_> = p.headers.get_all("x-a").iter().collect();
        assert_eq!(all, ["1", "3"]);
    }

    #[test]
    fn the_prepared_body_is_the_encoded_body() {
        let value = JsonValue::Object(vec![(
            "k".into(),
            JsonValue::Number(JsonNumber::UInt(u64::MAX)),
        )]);
        let mut want = Vec::new();
        crate::json::encode(&value, &mut want);
        let mut r = absolute("https://h/");
        r.body = Some(HttpBody::Json(value));
        assert_eq!(prepare(&r).unwrap().body.as_ref(), want.as_slice());

        r.body = Some(HttpBody::Text("héllo".into()));
        assert_eq!(prepare(&r).unwrap().body.as_ref(), "héllo".as_bytes());
        assert!(prepare(&absolute("https://h/")).unwrap().body.is_empty());
    }
}
