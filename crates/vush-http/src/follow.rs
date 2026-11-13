//! The redirect rules: which responses are followed, and what the next
//! request is.

use url::Url;
use vush_core::protocol::http::{HttpRequest, HttpResponse, RequestUrl};
use vush_core::protocol::{Failure, FailureKind, FailureStage};

use crate::prepare::request_url;

/// Headers dropped when a redirect changes the origin, in lowercase.
const CROSS_ORIGIN_DROPPED: [&str; 4] = ["authorization", "proxy-authorization", "cookie", "host"];

fn invalid(message: &'static str) -> Failure {
    Failure {
        kind: FailureKind::InvalidRedirect,
        stage: FailureStage::Prepare,
        message,
    }
}

/// The request that follows `response`, or `None` when `response` isn't a
/// redirect to follow. Pure: nothing is read but the arguments.
pub(crate) fn follow(
    request: &HttpRequest,
    response: &HttpResponse,
) -> Result<Option<HttpRequest>, Failure> {
    if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let mut locations = response
        .headers
        .iter()
        .filter(|(name, _)| name == "location");
    let Some((_, location)) = locations.next() else {
        return Ok(None);
    };
    if locations.next().is_some() {
        return Err(invalid("the redirect has more than one Location"));
    }
    let location = std::str::from_utf8(location)
        .map_err(|_| invalid("the redirect's Location can't be parsed"))?;

    // A request that was sent always rebuilds, so a failure here is not expected.
    let sent = request_url(request).map_err(|failure| invalid(failure.message))?;
    // `http://` fails to parse with `EmptyHost`, which is the no-host case.
    let mut target: Url = sent.join(location).map_err(|error| match error {
        url::ParseError::EmptyHost => invalid("the redirect's URL has no host"),
        _ => invalid("the redirect's Location can't be parsed"),
    })?;
    if !matches!(target.scheme(), "http" | "https") {
        return Err(invalid("the redirect goes to an unsupported scheme"));
    }
    if !target.username().is_empty() || target.password().is_some() {
        return Err(invalid("the redirect's URL holds a user name or password"));
    }
    if target.host_str().is_none_or(str::is_empty) {
        return Err(invalid("the redirect's URL has no host"));
    }
    target.set_fragment(None);

    let method = request.method.as_str();
    let (method, drop_body) = match response.status {
        303 if method == "HEAD" => ("HEAD", true),
        303 => ("GET", true),
        301 | 302 if method == "POST" => ("GET", true),
        _ => (method, false),
    };
    let mut headers = request.headers.clone();
    if drop_body {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
    }
    if target.origin() != sent.origin() {
        headers.retain(|(name, _)| {
            !CROSS_ORIGIN_DROPPED
                .iter()
                .any(|dropped| name.eq_ignore_ascii_case(dropped))
        });
    }
    Ok(Some(HttpRequest {
        method: method
            .parse()
            .map_err(|_| invalid("the redirect's Location can't be parsed"))?,
        url: RequestUrl::Absolute(target.into()),
        query: Vec::new(),
        headers,
        body: if drop_body {
            None
        } else {
            request.body.clone()
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vush_core::protocol::JsonValue;
    use vush_core::protocol::http::{HttpBody, WireVersion};

    fn response(status: u16, headers: &[(&str, &[u8])]) -> HttpResponse {
        HttpResponse {
            status,
            version: WireVersion::Http11,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.to_vec()))
                .collect(),
            body: Vec::new(),
            truncated: false,
        }
    }

    fn located(status: u16, location: &str) -> HttpResponse {
        response(status, &[("location", location.as_bytes())])
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    fn base(method: &str) -> HttpRequest {
        HttpRequest {
            method: method.parse().unwrap(),
            url: RequestUrl::Joined {
                base: "https://api.test/v1".to_owned(),
                path: "/a/b".to_owned(),
            },
            query: pairs(&[("q", "1")]),
            headers: pairs(&[("Content-Type", "application/json"), ("X-Keep", "1")]),
            body: Some(HttpBody::Json(JsonValue::Null)),
        }
    }

    fn next(request: &HttpRequest, response: &HttpResponse) -> HttpRequest {
        follow(request, response).unwrap().unwrap()
    }

    fn url_of(request: &HttpRequest) -> &str {
        match &request.url {
            RequestUrl::Absolute(url) => url,
            RequestUrl::Joined { .. } => panic!("the next URL must be absolute"),
        }
    }

    fn names(request: &HttpRequest) -> Vec<&str> {
        request.headers.iter().map(|(n, _)| n.as_str()).collect()
    }

    #[test]
    fn only_redirect_statuses_with_a_location_are_followed() {
        let request = base("GET");
        assert_eq!(follow(&request, &located(300, "/next")).unwrap(), None);
        assert_eq!(follow(&request, &located(304, "/next")).unwrap(), None);
        assert_eq!(follow(&request, &response(302, &[])).unwrap(), None);
        assert_eq!(follow(&request, &response(200, &[])).unwrap(), None);
        for status in [301, 302, 303, 307, 308] {
            assert!(
                follow(&request, &located(status, "/next"))
                    .unwrap()
                    .is_some(),
                "{status}"
            );
        }
    }

    #[test]
    fn method_and_body_follow_the_status() {
        let cases = [
            (303, "POST", "GET", false),
            (303, "HEAD", "HEAD", false),
            (303, "PUT", "GET", false),
            (301, "POST", "GET", false),
            (302, "POST", "GET", false),
            (302, "PUT", "PUT", true),
            (301, "DELETE", "DELETE", true),
            (307, "POST", "POST", true),
            (308, "PATCH", "PATCH", true),
        ];
        for (status, method, want, keeps) in cases {
            let got = next(&base(method), &located(status, "/next"));
            assert_eq!(got.method.as_str(), want, "{status} {method}");
            assert_eq!(got.body.is_some(), keeps, "{status} {method}");
            if keeps {
                assert_eq!(names(&got), ["Content-Type", "X-Keep"], "{status} {method}");
            } else {
                assert_eq!(names(&got), ["X-Keep"], "{status} {method}");
            }
        }
    }

    #[test]
    fn location_is_resolved_against_the_sent_url() {
        let request = base("GET");
        let cases = [
            ("c", "https://api.test/v1/a/c"),
            ("/root?x=1", "https://api.test/root?x=1"),
            ("//other.test/p", "https://other.test/p"),
            ("https://x.test/y#f", "https://x.test/y"),
        ];
        for (location, want) in cases {
            let got = next(&request, &located(302, location));
            assert_eq!(url_of(&got), want, "{location}");
            assert!(got.query.is_empty(), "{location}");
        }
    }

    #[test]
    fn locations_with_spaces_and_non_ascii_are_followed() {
        let got = next(&base("GET"), &located(302, "/a b/é"));
        assert_eq!(url_of(&got), "https://api.test/a%20b/%C3%A9");
    }

    #[test]
    fn bad_locations_are_invalid_redirects() {
        let request = base("GET");
        let two = response(302, &[("location", b"/a"), ("location", b"/b")]);
        let cases = [
            (two, "the redirect has more than one Location"),
            (
                response(302, &[("location", &[0xff])]),
                "the redirect's Location can't be parsed",
            ),
            (
                located(302, "ftp://h/x"),
                "the redirect goes to an unsupported scheme",
            ),
            (
                located(302, "https://u:p@h/"),
                "the redirect's URL holds a user name or password",
            ),
            (located(302, "http://"), "the redirect's URL has no host"),
        ];
        for (response, message) in cases {
            let failure = follow(&request, &response).unwrap_err();
            assert_eq!(failure.kind, FailureKind::InvalidRedirect, "{message}");
            assert_eq!(failure.stage, FailureStage::Prepare, "{message}");
            assert_eq!(failure.message, message);
        }
    }

    #[test]
    fn credentials_and_host_drop_on_an_origin_change() {
        let mut request = base("GET");
        request.headers = pairs(&[
            ("Authorization", "a"),
            ("proxy-authorization", "b"),
            ("COOKIE", "c"),
            ("Host", "d"),
            ("X-Keep", "1"),
        ]);
        request.body = None;
        for location in [
            "https://other.test/",
            "http://api.test/",
            "https://api.test:8443/",
        ] {
            let got = next(&request, &located(307, location));
            assert_eq!(names(&got), ["X-Keep"], "{location}");
        }
        let got = next(&request, &located(307, "/x"));
        assert_eq!(got.headers, request.headers);
    }

    #[test]
    fn default_ports_are_the_same_origin() {
        let mut request = base("GET");
        request.url = RequestUrl::Absolute("https://api.test:443/a".to_owned());
        request.headers = pairs(&[("Authorization", "a")]);
        let got = next(&request, &located(307, "https://api.test/b"));
        assert_eq!(names(&got), ["Authorization"]);
    }
}
