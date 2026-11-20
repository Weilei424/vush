//! Reading one `Set-Cookie` value (RFC 6265 §5.2) and the names a request's
//! own `Cookie` header sets.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::date::parse_date;

/// The most bytes a cookie's name and value may hold together.
pub(crate) const MAX_PAIR_BYTES: usize = 4096;
/// The most bytes a `Domain` or `Path` value may hold.
pub(crate) const MAX_ATTRIBUTE_BYTES: usize = 1024;

/// One `Set-Cookie` value that was read and not ignored.
pub(crate) struct SetCookie<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8],
    /// `None` means no expiry.
    pub expires: Option<SystemTime>,
    /// Lowercased, without its leading `.`.
    pub domain: Option<String>,
    /// Set only for a value that starts with `/`.
    pub path: Option<&'a [u8]>,
    pub secure: bool,
}

fn trim(mut text: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = text {
        text = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = text {
        text = rest;
    }
    text
}

fn split_pair(text: &[u8]) -> Option<(&[u8], &[u8])> {
    let eq = text.iter().position(|&b| b == b'=')?;
    Some((trim(&text[..eq]), trim(&text[eq + 1..])))
}

fn is_control(b: &u8) -> bool {
    matches!(b, 0x00..=0x08 | 0x0A..=0x1F | 0x7F)
}

/// Reads `Max-Age`: an optional `-`, then digits. Digits past `u64` saturate.
fn max_age(value: &[u8], now: SystemTime) -> Option<Option<SystemTime>> {
    let (negative, digits) = match value.strip_prefix(b"-") {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = digits.iter().fold(0u64, |acc, b| {
        acc.saturating_mul(10).saturating_add(u64::from(b - b'0'))
    });
    if negative || n == 0 {
        return Some(Some(UNIX_EPOCH));
    }
    Some(now.checked_add(Duration::from_secs(n)))
}

fn expires(value: &[u8]) -> Option<Option<SystemTime>> {
    let secs = parse_date(value)?;
    if secs <= 0 {
        return Some(Some(UNIX_EPOCH));
    }
    Some(UNIX_EPOCH.checked_add(Duration::from_secs(secs.unsigned_abs())))
}

/// Reads one `Set-Cookie` value. Gives `None` when the whole cookie is ignored.
pub(crate) fn parse(value: &[u8], now: SystemTime) -> Option<SetCookie<'_>> {
    let mut parts = value.split(|&b| b == b';');
    let (name, val) = split_pair(parts.next()?)?;
    if name.is_empty()
        || name.iter().any(is_control)
        || val.iter().any(is_control)
        || name.len() + val.len() > MAX_PAIR_BYTES
    {
        return None;
    }
    let mut max_age_seen: Option<Option<SystemTime>> = None;
    let mut expires_seen: Option<Option<SystemTime>> = None;
    let mut domain: Option<String> = None;
    let mut path: Option<&[u8]> = None;
    let mut secure = false;
    for attribute in parts {
        let (attr_name, attr_value) = match attribute.iter().position(|&b| b == b'=') {
            Some(_) => split_pair(attribute)?,
            None => (trim(attribute), &b""[..]),
        };
        if attr_name.eq_ignore_ascii_case(b"expires") {
            if let Some(e) = expires(attr_value) {
                expires_seen = Some(e);
            }
        } else if attr_name.eq_ignore_ascii_case(b"max-age") {
            if let Some(e) = max_age(attr_value, now) {
                max_age_seen = Some(e);
            }
        } else if attr_name.eq_ignore_ascii_case(b"domain") {
            if attr_value.is_empty() {
                continue;
            }
            if attr_value.len() > MAX_ATTRIBUTE_BYTES {
                return None;
            }
            let text = attr_value.strip_prefix(b".").unwrap_or(attr_value);
            if !text.is_ascii() || text.ends_with(b".") {
                return None;
            }
            if text.is_empty() {
                continue;
            }
            domain = Some(String::from_utf8_lossy(text).to_ascii_lowercase());
        } else if attr_name.eq_ignore_ascii_case(b"path") {
            if attr_value.len() > MAX_ATTRIBUTE_BYTES {
                return None;
            }
            path = attr_value.starts_with(b"/").then_some(attr_value);
        } else if attr_name.eq_ignore_ascii_case(b"secure") {
            secure = true;
        }
    }
    Some(SetCookie {
        name,
        value: val,
        expires: max_age_seen.or(expires_seen).flatten(),
        domain,
        path,
        secure,
    })
}

/// The names a request's own `Cookie` value sets. Pieces without `=` name nothing.
pub(crate) fn names(cookie_header: &[u8]) -> Vec<&[u8]> {
    cookie_header
        .split(|&b| b == b';')
        .filter_map(|piece| split_pair(piece).map(|(name, _)| name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_SECS: u64 = 1_000_000_000;
    static NOW: std::sync::LazyLock<SystemTime> =
        std::sync::LazyLock::new(|| UNIX_EPOCH + Duration::from_secs(NOW_SECS));

    fn at(secs: u64) -> Option<SystemTime> {
        Some(UNIX_EPOCH + Duration::from_secs(secs))
    }

    fn p(text: &str) -> Option<SetCookie<'_>> {
        parse(text.as_bytes(), *NOW)
    }

    fn exp(text: &str) -> Option<SystemTime> {
        p(text).unwrap().expires
    }

    #[test]
    fn names_and_values_are_trimmed() {
        let c = p(" a = 1 ").unwrap();
        assert_eq!((c.name, c.value), (&b"a"[..], &b"1"[..]));
        assert!(c.expires.is_none() && c.domain.is_none() && c.path.is_none() && !c.secure);
    }

    #[test]
    fn a_value_keeps_every_equals_sign_after_the_first() {
        assert_eq!(p("t=abc==").unwrap().value, b"abc==");
        assert_eq!(p("q=\"x y\"").unwrap().value, b"\"x y\"");
    }

    #[test]
    fn a_pair_needs_an_equals_sign_and_a_name() {
        for text in ["abc", "=1", " =1", ""] {
            assert!(p(text).is_none(), "{text:?}");
        }
    }

    #[test]
    fn control_characters_drop_the_cookie() {
        for text in ["a=1\x01", "a\x7f=1", "a=\x1f"] {
            assert!(p(text).is_none(), "{text:?}");
        }
        assert_eq!(p("a=1\t2").unwrap().value, b"1\t2");
    }

    #[test]
    fn the_pair_is_at_most_4096_bytes() {
        assert!(p(&format!("a={}", "v".repeat(4095))).is_some());
        assert!(p(&format!("a={}", "v".repeat(4096))).is_none());
    }

    #[test]
    fn attribute_names_ignore_case() {
        let c = p("a=1; SECURE; path=/x; DoMaIn=Vush.Test").unwrap();
        assert!(c.secure);
        assert_eq!(c.path, Some(&b"/x"[..]));
        assert_eq!(c.domain.as_deref(), Some("vush.test"));
    }

    #[test]
    fn the_last_valid_attribute_wins() {
        assert_eq!(p("a=1; Path=/x; Path=/y").unwrap().path, Some(&b"/y"[..]));
        assert_eq!(exp("a=1; Max-Age=10; Max-Age=junk"), at(1_000_000_010));
        assert_eq!(exp("a=1; Max-Age=10; Max-Age=20"), at(1_000_000_020));
        assert_eq!(p("a=1; Path=/a; Path=x").unwrap().path, None);
        assert_eq!(
            p("a=1; Domain=a.test; Domain=").unwrap().domain.as_deref(),
            Some("a.test")
        );
    }

    #[test]
    fn max_age_rules() {
        assert_eq!(exp("a=1; Max-Age=-5"), Some(UNIX_EPOCH));
        assert_eq!(exp("a=1; Max-Age=0"), Some(UNIX_EPOCH));
        assert_eq!(exp("a=1; Max-Age=+5"), None);
        assert_eq!(exp("a=1; Max-Age=5s"), None);
        assert_eq!(exp("a=1; Max-Age="), None);
        assert_eq!(exp("a=1; Max-Age=99999999999999999999"), None);
        assert_eq!(
            exp("a=1; Max-Age=99999999999999999999; Expires=Sun, 06 Nov 1994 08:49:37 GMT"),
            None
        );
    }

    #[test]
    fn max_age_beats_expires() {
        let date = "Expires=Sun, 06 Nov 1994 08:49:37 GMT";
        assert_eq!(exp(&format!("a=1; Max-Age=10; {date}")), at(1_000_000_010));
        assert_eq!(exp(&format!("a=1; {date}; Max-Age=10")), at(1_000_000_010));
    }

    #[test]
    fn expires_rules() {
        assert_eq!(
            exp("a=1; Expires=Sun, 06 Nov 1994 08:49:37 GMT"),
            at(784_111_777)
        );
        assert_eq!(
            exp("a=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(UNIX_EPOCH)
        );
        assert_eq!(exp("a=1; Expires=01 Jan 1601 00:00:00"), Some(UNIX_EPOCH));
        assert_eq!(exp("a=1; Expires=junk"), None);
    }

    #[test]
    fn domain_rules() {
        let domain = |t: &str| p(t).map(|c| c.domain);
        assert_eq!(domain("a=1; Domain="), Some(None));
        assert_eq!(
            domain("a=1; Domain=.vush.test"),
            Some(Some("vush.test".into()))
        );
        assert!(p("a=1; Domain=vush.test.").is_none());
        assert!(p("a=1; Domain=vüsh.test").is_none());
        let long = |n: usize| format!("a=1; Domain={}", "d".repeat(n));
        assert!(p(&long(1024)).is_some());
        assert!(p(&long(1025)).is_none());
        assert!(p("a=1; Domain=a.test; Domain=b.test.").is_none());
    }

    #[test]
    fn path_rules() {
        assert_eq!(p("a=1; Path=").unwrap().path, None);
        assert_eq!(p("a=1; Path=x").unwrap().path, None);
        assert_eq!(p("a=1; Path=/a").unwrap().path, Some(&b"/a"[..]));
        let long = |n: usize| format!("a=1; Path=/{}", "p".repeat(n - 1));
        assert!(p(&long(1024)).is_some());
        assert!(p(&long(1025)).is_none());
    }

    #[test]
    fn browser_only_attributes_are_skipped() {
        let c = p("a=1; HttpOnly; SameSite=Strict; Partitioned; Foo=bar").unwrap();
        assert_eq!((c.name, c.value), (&b"a"[..], &b"1"[..]));
        assert!(c.expires.is_none() && c.domain.is_none() && c.path.is_none() && !c.secure);
    }

    #[test]
    fn request_cookie_names() {
        assert_eq!(names(b"a=1; b =2;c; =3; d="), [&b"a"[..], b"b", b"", b"d"]);
        assert!(names(b"").is_empty());
    }
}
