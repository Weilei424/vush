//! The jar's rules, from RFC 6265 §5: the cookie-date and `Set-Cookie`
//! parsers, and a per-user jar that stores, matches, orders and evicts cookies.

mod date;
mod parse;

use std::num::NonZeroU32;
use std::time::SystemTime;

use url::{Host, Url};

/// What the jar needs to know about the URL a request is sent to.
pub(crate) struct Target {
    pub(crate) secure: bool,
    /// Lowercase ASCII, with an IPv6 address in brackets.
    pub(crate) host: String,
    pub(crate) ip: bool,
    /// The URL's path, without the query.
    pub(crate) path: String,
}

impl Target {
    pub(crate) fn of(url: &Url) -> Self {
        Self {
            secure: url.scheme() == "https",
            host: url.host_str().unwrap_or("").to_owned(),
            ip: matches!(url.host(), Some(Host::Ipv4(_) | Host::Ipv6(_))),
            path: url.path().to_owned(),
        }
    }
}

#[derive(Clone)]
struct Stored {
    name: Vec<u8>,
    value: Vec<u8>,
    /// The host for a host-only cookie, the domain otherwise.
    domain: String,
    host_only: bool,
    path: Vec<u8>,
    secure: bool,
    expires: Option<SystemTime>,
    /// Taken from the jar's counter when the key first appears; also the ID.
    created: u64,
    /// Taken from the jar's counter when the cookie is set or sent.
    used: u64,
}

impl Stored {
    fn expired(&self, now: SystemTime) -> bool {
        self.expires.is_some_and(|at| at <= now)
    }

    fn matches(&self, target: &Target) -> bool {
        let host_ok = if self.host_only {
            self.domain == target.host
        } else {
            !target.ip && domain_matches(&target.host, &self.domain)
        };
        host_ok
            && (!self.secure || target.secure)
            && path_matches(&self.path, target.path.as_bytes())
    }
}

/// `host` is `domain` or ends with `.` plus `domain`.
fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|rest| rest.ends_with('.'))
}

fn path_matches(cookie: &[u8], request: &[u8]) -> bool {
    request.starts_with(cookie)
        && (request.len() == cookie.len()
            || cookie.ends_with(b"/")
            || request[cookie.len()] == b'/')
}

/// RFC 6265 §5.1.4.
fn default_path(request_path: &str) -> &[u8] {
    let bytes = request_path.as_bytes();
    match bytes.iter().rposition(|&b| b == b'/') {
        Some(last) if last > 0 && bytes[0] == b'/' => &bytes[..last],
        _ => b"/",
    }
}

/// One user's cookies.
#[derive(Clone)]
pub(crate) struct Jar {
    cookies: Vec<Stored>,
    max: usize,
    counter: u64,
}

/// The cookies a hop would send, not yet counted as used.
pub(crate) struct Picked {
    pairs: Vec<u8>,
    ids: Vec<u64>,
}

impl Picked {
    /// The `name=value` pairs, joined with `; `.
    pub(crate) fn pairs(&self) -> &[u8] {
        &self.pairs
    }
}

impl Jar {
    pub(crate) fn new(max: NonZeroU32) -> Self {
        Self {
            cookies: Vec::new(),
            max: max.get() as usize,
            counter: 0,
        }
    }

    fn tick(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    /// Stores one `Set-Cookie` value received from `target` (RFC 6265 §5.3).
    pub(crate) fn store(&mut self, set_cookie: &[u8], target: &Target, now: SystemTime) {
        let Some(parsed) = parse::parse(set_cookie, now) else {
            return;
        };
        let (domain, host_only) = match &parsed.domain {
            None => (target.host.clone(), true),
            Some(d) if target.ip => {
                if *d != target.host {
                    return;
                }
                (d.clone(), true)
            }
            Some(d) if !domain_matches(&target.host, d) => return,
            Some(d) if !d.contains('.') => {
                if *d != target.host {
                    return;
                }
                (d.clone(), true)
            }
            Some(d) => (d.clone(), false),
        };
        if parsed.secure && !target.secure {
            return;
        }
        let path = parsed
            .path
            .map_or_else(|| default_path(&target.path), |p| p)
            .to_vec();
        let existing = self
            .cookies
            .iter()
            .position(|c| c.name == parsed.name && c.domain == domain && c.path == path);
        if parsed.expires.is_some_and(|at| at <= now) {
            if let Some(i) = existing {
                self.cookies.remove(i);
            }
            return;
        }
        let used = self.tick();
        let (value, secure, expires) = (parsed.value.to_vec(), parsed.secure, parsed.expires);
        if let Some(i) = existing {
            let c = &mut self.cookies[i];
            c.value = value;
            c.host_only = host_only;
            c.secure = secure;
            c.expires = expires;
            c.used = used;
            return;
        }
        if self.cookies.len() >= self.max {
            self.cookies.retain(|c| !c.expired(now));
        }
        while self.cookies.len() >= self.max {
            let Some(oldest) = (0..self.cookies.len())
                .min_by_key(|&i| (self.cookies[i].used, self.cookies[i].created))
            else {
                break;
            };
            self.cookies.remove(oldest);
        }
        self.cookies.push(Stored {
            name: parsed.name.to_vec(),
            value,
            domain,
            host_only,
            path,
            secure,
            expires,
            created: used,
            used,
        });
    }

    /// Removes expired cookies, then picks those that go to `target`: longest
    /// path first, then oldest first, without any name in `own`. Marks nothing.
    pub(crate) fn pick(
        &mut self,
        target: &Target,
        own: &[&[u8]],
        now: SystemTime,
    ) -> Option<Picked> {
        self.cookies.retain(|c| !c.expired(now));
        let own: Vec<&[u8]> = own.iter().flat_map(|v| parse::names(v)).collect();
        let mut chosen: Vec<&Stored> = self
            .cookies
            .iter()
            .filter(|c| c.matches(target) && !own.contains(&c.name.as_slice()))
            .collect();
        if chosen.is_empty() {
            return None;
        }
        chosen.sort_by(|a, b| {
            b.path
                .len()
                .cmp(&a.path.len())
                .then(a.created.cmp(&b.created))
        });
        let mut pairs = Vec::new();
        for (i, c) in chosen.iter().enumerate() {
            if i > 0 {
                pairs.extend_from_slice(b"; ");
            }
            pairs.extend_from_slice(&c.name);
            pairs.push(b'=');
            pairs.extend_from_slice(&c.value);
        }
        let ids = chosen.iter().map(|c| c.created).collect();
        Some(Picked { pairs, ids })
    }

    /// Counts every cookie in `picked` as used now, all with one new value.
    pub(crate) fn mark_used(&mut self, picked: &Picked) {
        let used = self.tick();
        for c in &mut self.cookies {
            if picked.ids.contains(&c.created) {
                c.used = used;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000_000)
    }

    fn later(secs: u64) -> SystemTime {
        now() + Duration::from_secs(secs)
    }

    fn jar(max: u32) -> Jar {
        Jar::new(NonZeroU32::new(max).unwrap())
    }

    fn target(url: &str) -> Target {
        Target::of(&Url::parse(url).unwrap())
    }

    fn put(jar: &mut Jar, url: &str, set_cookie: &str) {
        jar.store(set_cookie.as_bytes(), &target(url), now());
    }

    fn sent(jar: &mut Jar, url: &str) -> String {
        sent_at(jar, url, now())
    }

    fn sent_at(jar: &mut Jar, url: &str, at: SystemTime) -> String {
        match jar.pick(&target(url), &[], at) {
            Some(picked) => {
                jar.mark_used(&picked);
                String::from_utf8(picked.pairs().to_vec()).unwrap()
            }
            None => String::new(),
        }
    }

    #[test]
    fn targets_use_the_normalized_host() {
        let t = target("http://A.Vush.TEST/x?y");
        assert_eq!((t.host.as_str(), t.path.as_str()), ("a.vush.test", "/x"));
        assert!(!t.secure && !t.ip);
        let t = target("https://127.0.0.1/");
        assert!(t.secure && t.ip);
        let t = target("http://[::1]/");
        assert!(t.ip && t.host == "[::1]");
    }

    #[test]
    fn host_only_and_domain_cookies() {
        let mut j = jar(64);
        put(&mut j, "http://a.vush.test/", "h=1");
        put(&mut j, "http://a.vush.test/", "d=1; Domain=vush.test");
        assert_eq!(sent(&mut j, "http://a.vush.test/"), "h=1; d=1");
        assert_eq!(sent(&mut j, "http://b.vush.test/"), "d=1");
        assert_eq!(sent(&mut j, "http://vush.test/"), "d=1");
        assert_eq!(sent(&mut j, "http://other.test/"), "");
        assert_eq!(sent(&mut j, "http://xvush.test/"), "");
    }

    #[test]
    fn a_domain_must_hold_the_host() {
        let mut j = jar(64);
        put(&mut j, "http://a.vush.test/", "a=1; Domain=other.test");
        assert_eq!(sent(&mut j, "http://other.test/"), "");
        assert_eq!(sent(&mut j, "http://a.vush.test/"), "");
        assert!(j.cookies.is_empty());
    }

    #[test]
    fn one_label_domains() {
        let mut j = jar(64);
        put(&mut j, "http://a.test/", "a=1; Domain=test");
        assert!(j.cookies.is_empty());
        put(&mut j, "http://localhost/", "l=1; Domain=localhost");
        assert_eq!(sent(&mut j, "http://localhost/"), "l=1");
        assert_eq!(sent(&mut j, "http://a.localhost/"), "");
    }

    #[test]
    fn ip_hosts_get_exact_cookies() {
        let mut j = jar(64);
        put(&mut j, "http://127.0.0.1/", "a=1; Domain=127.0.0.1");
        assert_eq!(j.cookies.len(), 1);
        assert!(j.cookies[0].host_only);
        put(&mut j, "http://127.0.0.1/", "b=1; Domain=0.0.1");
        assert_eq!(j.cookies.len(), 1);
        assert_eq!(sent(&mut j, "http://127.0.0.1/"), "a=1");
        assert_eq!(sent(&mut j, "http://10.0.0.1/"), "");
        let mut j = jar(64);
        j.cookies.push(Stored {
            name: b"d".to_vec(),
            value: b"1".to_vec(),
            domain: "0.0.1".into(),
            host_only: false,
            path: b"/".to_vec(),
            secure: false,
            expires: None,
            created: 1,
            used: 1,
        });
        assert_eq!(sent(&mut j, "http://10.0.0.1/"), "");
    }

    #[test]
    fn default_paths() {
        for (url, want) in [
            ("http://h.test/", "/"),
            ("http://h.test/a", "/"),
            ("http://h.test/a/b/c", "/a/b"),
        ] {
            let mut j = jar(64);
            put(&mut j, url, "a=1");
            assert_eq!(j.cookies[0].path, want.as_bytes(), "{url}");
        }
    }

    #[test]
    fn path_matching() {
        let mut j = jar(64);
        put(&mut j, "http://h.test/", "a=1; Path=/a");
        for url in ["http://h.test/a", "http://h.test/a/", "http://h.test/a/b"] {
            assert_eq!(sent(&mut j, url), "a=1", "{url}");
        }
        for url in ["http://h.test/ab", "http://h.test/"] {
            assert_eq!(sent(&mut j, url), "", "{url}");
        }
        let mut j = jar(64);
        put(&mut j, "http://h.test/", "a=1; Path=/a/");
        assert_eq!(sent(&mut j, "http://h.test/a/b"), "a=1");
        assert_eq!(sent(&mut j, "http://h.test/a"), "");
    }

    #[test]
    fn secure_cookies() {
        let mut j = jar(64);
        put(&mut j, "https://h.test/", "s=1; Secure");
        assert_eq!(sent(&mut j, "https://h.test/"), "s=1");
        assert_eq!(sent(&mut j, "http://h.test/"), "");
        let mut j = jar(64);
        put(&mut j, "http://h.test/", "x=1; Secure");
        assert!(j.cookies.is_empty());
    }

    #[test]
    fn send_order_is_longest_path_then_oldest() {
        let mut j = jar(64);
        put(&mut j, "http://h.test/", "a=1; Path=/");
        put(&mut j, "http://h.test/", "b=1; Path=/x");
        put(&mut j, "http://h.test/", "c=1; Path=/");
        assert_eq!(sent(&mut j, "http://h.test/x/y"), "b=1; a=1; c=1");
        let mut j = jar(64);
        put(&mut j, "http://h.test/", "n=1; Path=/");
        put(&mut j, "http://h.test/", "n=2; Path=/x");
        assert_eq!(sent(&mut j, "http://h.test/x"), "n=2; n=1");
    }

    #[test]
    fn replacing_keeps_the_creation_order() {
        let mut j = jar(64);
        put(&mut j, "http://h.test/", "a=1");
        put(&mut j, "http://h.test/", "b=1");
        put(&mut j, "http://h.test/", "a=2");
        assert_eq!(sent(&mut j, "http://h.test/"), "a=2; b=1");
    }

    #[test]
    fn expiry_deletes_and_expired_cookies_are_never_sent() {
        let mut j = jar(64);
        put(&mut j, "http://h.test/", "a=1");
        put(&mut j, "http://h.test/", "a=1; Max-Age=0");
        assert!(j.cookies.is_empty());
        put(&mut j, "http://h.test/", "a=1; Max-Age=10");
        assert_eq!(sent_at(&mut j, "http://h.test/", later(9)), "a=1");
        assert_eq!(sent_at(&mut j, "http://h.test/", later(10)), "");
        assert!(j.cookies.is_empty());
        put(&mut j, "http://h.test/", "a=1");
        put(
            &mut j,
            "http://h.test/",
            "a=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
        );
        assert!(j.cookies.is_empty());
    }

    #[test]
    fn the_request_wins_on_names() {
        let mut j = jar(64);
        put(&mut j, "http://h.test/", "a=jar");
        put(&mut j, "http://h.test/", "b=jar");
        let t = target("http://h.test/");
        let p = j.pick(&t, &[b"a=req"], now()).unwrap();
        assert_eq!(p.pairs(), b"b=jar");
        let p = j.pick(&t, &[b"A=req"], now()).unwrap();
        assert_eq!(p.pairs(), b"a=jar; b=jar");
        assert!(j.pick(&t, &[b"a=1", b"b=2"], now()).is_none());
    }

    #[test]
    fn eviction_takes_expired_then_least_recently_used() {
        let mut j = jar(2);
        put(&mut j, "http://h.test/", "a=1; Max-Age=5");
        put(&mut j, "http://h.test/", "b=1");
        j.store(b"c=1", &target("http://h.test/"), later(6));
        assert_eq!(sent_at(&mut j, "http://h.test/", later(6)), "b=1; c=1");
        let mut j = jar(2);
        put(&mut j, "http://h.test/", "a=1; Path=/x");
        put(&mut j, "http://h.test/", "b=1; Path=/");
        assert_eq!(sent(&mut j, "http://h.test/"), "b=1");
        put(&mut j, "http://h.test/", "c=1");
        assert_eq!(sent(&mut j, "http://h.test/x"), "b=1; c=1");
    }

    #[test]
    fn one_response_past_the_limit_keeps_the_last() {
        let mut j = jar(2);
        for n in 1..=5 {
            put(&mut j, "http://h.test/", &format!("c{n}=1"));
        }
        assert_eq!(sent(&mut j, "http://h.test/"), "c4=1; c5=1");
    }

    #[test]
    fn replacing_at_the_limit_evicts_nothing() {
        let mut j = jar(2);
        put(&mut j, "http://h.test/", "a=1");
        put(&mut j, "http://h.test/", "b=1");
        put(&mut j, "http://h.test/", "a=2");
        assert_eq!(sent(&mut j, "http://h.test/"), "a=2; b=1");
    }

    #[test]
    fn the_limit_holds_3000() {
        let mut j = jar(3000);
        for n in 0..=3000 {
            put(&mut j, "http://h.test/", &format!("n{n}=1"));
        }
        assert_eq!(j.cookies.len(), 3000);
        assert!(j.cookies.iter().all(|c| c.name != b"n0"));
        assert!(j.cookies.iter().any(|c| c.name == b"n1"));
    }

    #[test]
    fn a_copy_has_its_own_cookies() {
        let mut original = jar(64);
        put(&mut original, "http://h.test/", "a=1");
        let mut copy = original.clone();
        put(&mut original, "http://h.test/", "b=1");
        put(&mut copy, "http://h.test/", "c=1");
        assert_eq!(sent(&mut original, "http://h.test/"), "a=1; b=1");
        assert_eq!(sent(&mut copy, "http://h.test/"), "a=1; c=1");
    }

    #[test]
    fn picking_marks_nothing_until_asked() {
        let mut j = jar(2);
        put(&mut j, "http://h.test/", "a=1; Path=/x");
        put(&mut j, "http://h.test/", "b=1; Path=/y");
        let p = j.pick(&target("http://h.test/x"), &[], now()).unwrap();
        assert_eq!(p.pairs(), b"a=1");
        put(&mut j, "http://h.test/", "c=1; Path=/z");
        assert_eq!(sent(&mut j, "http://h.test/x"), "");
        assert_eq!(sent(&mut j, "http://h.test/y"), "b=1");
    }
}
