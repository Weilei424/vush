//! Turns the located value tree into a plan draft.

use std::fmt::Write as _;

mod load;
mod reader;
mod settings;
mod step;
mod threshold;

use vush_core::plan::{
    HttpSettings, JsonNumber, PathSegment, PlanDraft, PlanPath, ResourceLimits, Scenario,
    StatePolicy, Step, Template,
};

use self::reader::{MapReader, child, item, list, one_of, parsed, string};
use crate::error::{Errors, Location, push_display_key};
use crate::node::{Node, NodeKind};

/// Builds the draft. Returns `None` only when something was recorded.
/// Stops early only for a bad `schema_version`, because the rest of the file
/// may then follow a different format.
pub(crate) fn compile(root: &Node, errors: &mut Errors) -> Option<PlanDraft> {
    let mut map = MapReader::new(root, "", errors)?;
    let version = map.required("schema_version", errors)?;
    match &version.kind {
        NodeKind::Invalid => return None,
        NodeKind::Number(JsonNumber::Int(1) | JsonNumber::UInt(1)) => {}
        _ => {
            errors.push(
                "schema_version",
                Some(version.location),
                "schema_version must be 1",
            );
            return None;
        }
    }

    let mut ok = true;
    let name = map
        .required("name", errors)
        .and_then(|n| string(n, "name", errors))
        .map(str::to_owned);
    ok &= name.is_some();

    let base_url = match map.optional("base_url") {
        Some(n) => {
            let url = parsed(n, "base_url", str::parse::<Template>, errors);
            ok &= url.is_some();
            url
        }
        None => None,
    };

    let http = match map.optional("http") {
        Some(n) => settings::http(n, "http", &HttpSettings::default(), errors),
        None => HttpSettings::default(),
    };
    let (limits, max_request_rate) = match map.optional("limits") {
        Some(n) => settings::limits(n, "limits", &ResourceLimits::default(), errors),
        None => (ResourceLimits::default(), None),
    };

    let setup = hook(&mut map, "setup", http.timeout, errors, &mut ok);
    let teardown = hook(&mut map, "teardown", http.timeout, errors, &mut ok);

    let thresholds = match map.optional("thresholds") {
        Some(n) => {
            let list = threshold::thresholds(n, "thresholds", errors);
            ok &= list.is_some();
            list.unwrap_or_default()
        }
        None => Vec::new(),
    };

    let mut scenarios = Vec::new();
    if let Some(n) = map.required("scenarios", errors) {
        match list(n, "scenarios", errors) {
            Some(items) => {
                for (index, element) in items.iter().enumerate() {
                    match scenario(element, &item("scenarios", index), &http, &limits, errors) {
                        Some(built) => scenarios.push(built),
                        None => ok = false,
                    }
                }
            }
            None => ok = false,
        }
    } else {
        ok = false;
    }
    map.finish(errors);

    if !ok {
        return None;
    }
    Some(PlanDraft {
        name: name?,
        base_url,
        http,
        limits,
        max_request_rate,
        setup,
        teardown,
        thresholds,
        scenarios,
    })
}

/// Reads an optional step list, empty when absent.
fn hook(
    map: &mut MapReader<'_>,
    key: &str,
    timeout: std::time::Duration,
    errors: &mut Errors,
    ok: &mut bool,
) -> Vec<Step> {
    let Some(n) = map.optional(key) else {
        return Vec::new();
    };
    let path = child(map.path(), key);
    match step::steps(n, &path, timeout, errors) {
        Some(steps) => steps,
        None => {
            *ok = false;
            Vec::new()
        }
    }
}

fn scenario(
    node: &Node,
    path: &str,
    file_http: &HttpSettings,
    file_limits: &ResourceLimits,
    errors: &mut Errors,
) -> Option<Scenario> {
    let mut map = MapReader::new(node, path, errors)?;
    let mut ok = true;

    let name = map
        .required("name", errors)
        .and_then(|n| string(n, &child(path, "name"), errors))
        .map(str::to_owned);
    ok &= name.is_some();

    let load = map
        .required("load", errors)
        .and_then(|n| load::load(n, &child(path, "load"), errors));
    ok &= load.is_some();

    let http = match map.optional("http") {
        Some(n) => settings::http(n, &child(path, "http"), file_http, errors),
        None => file_http.clone(),
    };
    let (limits, max_request_rate) = match map.optional("limits") {
        Some(n) => settings::limits(n, &child(path, "limits"), file_limits, errors),
        None => (file_limits.clone(), None),
    };

    let state = match map.optional("state") {
        Some(n) => {
            let choices = [
                ("persist", StatePolicy::Persist),
                ("reset_each_iteration", StatePolicy::ResetEachIteration),
            ];
            let state = one_of(n, &child(path, "state"), &choices, errors);
            ok &= state.is_some();
            state.unwrap_or(StatePolicy::Persist)
        }
        None => StatePolicy::Persist,
    };

    let timeout = http.timeout;
    let setup = hook(&mut map, "setup", timeout, errors, &mut ok);
    let teardown = hook(&mut map, "teardown", timeout, errors, &mut ok);
    let vu_setup = hook(&mut map, "vu_setup", timeout, errors, &mut ok);
    let vu_teardown = hook(&mut map, "vu_teardown", timeout, errors, &mut ok);

    let steps = map
        .required("steps", errors)
        .and_then(|n| step::steps(n, &child(path, "steps"), timeout, errors));
    ok &= steps.is_some();
    map.finish(errors);

    if !ok {
        return None;
    }
    Some(Scenario {
        name: name?,
        load: load?,
        http,
        limits,
        max_request_rate,
        state,
        setup,
        teardown,
        vu_setup,
        vu_teardown,
        steps: steps?,
    })
}

/// Finds the value a plan error path points at, or the deepest one reached.
/// With `at_key`, a path that ends at a matching map key gives the key's own
/// location instead of its value's. A cut key matches the first entry whose
/// key starts with it and is longer.
pub(crate) fn locate(root: &Node, path: &PlanPath, at_key: bool) -> Location {
    let mut at = root;
    let segments = path.segments();
    for (position, segment) in segments.iter().enumerate() {
        let found = match (segment, &at.kind) {
            (PathSegment::Key(key), NodeKind::Map(entries)) => {
                entries.iter().find(|e| e.key == *key)
            }
            (PathSegment::TruncatedKey(prefix), NodeKind::Map(entries)) => entries
                .iter()
                .find(|e| e.key.len() > prefix.len() && e.key.starts_with(prefix.as_str())),
            (PathSegment::Index(index), NodeKind::List(items)) => match items.get(*index) {
                Some(next) => {
                    at = next;
                    continue;
                }
                None => return at.location,
            },
            _ => return at.location,
        };
        let Some(entry) = found else {
            return at.location;
        };
        if at_key && position + 1 == segments.len() {
            return entry.key_location;
        }
        at = &entry.value;
    }
    at.location
}

/// Writes a plan error path the way compile errors write theirs, with each
/// key escaped. A cut key shows as its whole key would: 64 characters and `…`.
pub(crate) fn render_path(path: &PlanPath) -> String {
    let mut text = String::new();
    for segment in path.segments() {
        let (key, cut) = match segment {
            PathSegment::Key(key) => (key, false),
            PathSegment::TruncatedKey(prefix) => (prefix, true),
            PathSegment::Index(index) => {
                // Writing to a `String` can't fail.
                let _ = write!(text, "[{index}]");
                continue;
            }
        };
        if !text.is_empty() {
            text.push('.');
        }
        push_display_key(&mut text, key, cut);
    }
    text
}

#[cfg(test)]
pub(crate) mod testing {
    use crate::error::{ConfigError, Errors};
    use crate::node::Node;

    /// Reads YAML into a tree, panicking if the reader reports errors.
    pub(crate) fn node(yaml: &str) -> Node {
        let mut errors = Errors::new();
        let node = crate::yaml::read(yaml, &mut errors);
        assert!(
            errors.is_empty(),
            "reader errors: {:?}",
            errors.into_sorted()
        );
        node.expect("the reader returned no tree")
    }

    /// Runs `f` on the tree and returns its result with the errors it recorded.
    pub(crate) fn run<T>(
        yaml: &str,
        f: impl FnOnce(&Node, &mut Errors) -> T,
    ) -> (T, Vec<ConfigError>) {
        let tree = node(yaml);
        let mut errors = Errors::new();
        let value = f(&tree, &mut errors);
        (value, errors.into_sorted())
    }

    /// Asserts exactly one error, with this path and message fragment.
    pub(crate) fn assert_fails<T>(
        yaml: &str,
        path: &str,
        fragment: &str,
        f: impl FnOnce(&Node, &mut Errors) -> T,
    ) {
        let (_, errors) = run(yaml, f);
        assert_eq!(errors.len(), 1, "{yaml}: {errors:?}");
        assert_eq!(errors[0].path, path, "{yaml}");
        assert!(
            errors[0].message.contains(fragment),
            "{yaml}: {:?} lacks {fragment:?}",
            errors[0].message
        );
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};
    use std::time::Duration;

    use vush_core::plan::{
        ClosedStop, ClosedVusLoad, HttpVersion, LoadModel, Rate, parse_duration,
    };

    use super::testing::run;
    use super::*;
    use crate::error::ConfigError;

    const MINIMAL: &str = "\
schema_version: 1
name: n
scenarios:
  - name: s
    load: {model: closed_vus, vus: 1, duration: 1s}
    steps: [{wait: 1s}]
";

    fn d(text: &str) -> Duration {
        parse_duration(text).unwrap()
    }

    fn rate(text: &str) -> Rate {
        text.parse().unwrap()
    }

    fn build(yaml: &str) -> (Option<PlanDraft>, Vec<ConfigError>) {
        run(yaml, compile)
    }

    fn ok(yaml: &str) -> PlanDraft {
        let (draft, errors) = build(yaml);
        assert!(errors.is_empty(), "{yaml}: {errors:?}");
        draft.expect("no draft")
    }

    fn fails(yaml: &str, path: &str, fragment: &str) {
        let (_, errors) = build(yaml);
        assert_eq!(errors.len(), 1, "{yaml}: {errors:?}");
        assert_eq!(errors[0].path, path, "{yaml}");
        assert!(errors[0].message.contains(fragment), "{yaml}: {errors:?}");
    }

    /// MINIMAL with extra top-level text added after the header.
    fn with_top(extra: &str) -> String {
        format!("{extra}\n{MINIMAL}")
    }

    fn timeouts(steps: &[Step]) -> Vec<Duration> {
        steps
            .iter()
            .map(|s| match s {
                Step::Request(r) => r.timeout,
                _ => panic!("not a request"),
            })
            .collect()
    }

    #[test]
    fn a_minimal_file_takes_every_default() {
        let want = PlanDraft {
            name: "n".to_owned(),
            base_url: None,
            http: HttpSettings::default(),
            limits: ResourceLimits::default(),
            max_request_rate: None,
            setup: vec![],
            teardown: vec![],
            thresholds: vec![],
            scenarios: vec![Scenario {
                name: "s".to_owned(),
                load: LoadModel::ClosedVus(ClosedVusLoad::Fixed {
                    vus: NonZeroU32::new(1).unwrap(),
                    stop: ClosedStop::Duration(d("1s")),
                }),
                http: HttpSettings::default(),
                limits: ResourceLimits::default(),
                max_request_rate: None,
                state: StatePolicy::Persist,
                setup: vec![],
                teardown: vec![],
                vu_setup: vec![],
                vu_teardown: vec![],
                steps: vec![Step::Wait(d("1s"))],
            }],
        };
        assert_eq!(ok(MINIMAL), want);
        let _ = NonZeroU64::new(1);
    }

    #[test]
    fn schema_version_is_checked_first() {
        for value in ["2", "\"1\"", "1.0"] {
            fails(
                &format!("schema_version: {value}\nnmae: x\n"),
                "schema_version",
                "schema_version must be 1",
            );
        }
        fails("nmae: x\n", "", "missing required key `schema_version`");
    }

    #[test]
    fn the_top_level_must_be_a_map() {
        fails("[1]", "", "expected a map, found a list");
    }

    #[test]
    fn empty_block_is_an_error_not_absent() {
        for key in ["http", "limits", "setup", "teardown", "thresholds"] {
            fails(&with_top(&format!("{key}:")), key, "found null");
        }
    }

    #[test]
    fn timeouts_follow_the_http_in_effect() {
        let req = "{request: {method: GET, url: http://x}}";
        let yaml = format!(
            "\
schema_version: 1
name: n
http: {{timeout: 10s}}
setup: [{req}]
teardown: [{req}]
scenarios:
  - name: s
    http: {{timeout: 2s}}
    load: {{model: closed_vus, vus: 1, duration: 1s}}
    setup: [{req}]
    teardown: [{req}]
    vu_setup: [{req}]
    vu_teardown: [{req}]
    steps: [{req}]
  - name: t
    load: {{model: closed_vus, vus: 1, duration: 1s}}
    steps: [{req}]
"
        );
        let draft = ok(&yaml);
        assert_eq!(timeouts(&draft.setup), [d("10s")]);
        assert_eq!(timeouts(&draft.teardown), [d("10s")]);
        let s = &draft.scenarios[0];
        for steps in [&s.setup, &s.teardown, &s.vu_setup, &s.vu_teardown, &s.steps] {
            assert_eq!(timeouts(steps), [d("2s")]);
        }
        assert_eq!(timeouts(&draft.scenarios[1].steps), [d("10s")]);
    }

    #[test]
    fn scenario_http_merges_over_the_file_level() {
        let yaml = "\
schema_version: 1
name: n
http: {version: http2, tls: {ca_file: a.pem}}
scenarios:
  - name: s
    http: {tls: {insecure: true}}
    load: {model: closed_vus, vus: 1, duration: 1s}
    steps: [{wait: 1s}]
";
        let http = &ok(yaml).scenarios[0].http;
        assert_eq!(http.version, HttpVersion::Http2);
        assert_eq!(http.tls.ca_file.as_deref(), Some("a.pem"));
        assert!(http.tls.insecure);
    }

    #[test]
    fn limits_inherit_but_the_rate_budget_does_not() {
        let yaml = "\
schema_version: 1
name: n
limits: {max_request_rate: 500/s, max_variables: 128}
scenarios:
  - name: s
    limits: {max_cookies: 32}
    load: {model: closed_vus, vus: 1, duration: 1s}
    steps: [{wait: 1s}]
  - name: t
    limits: {max_request_rate: 100/s}
    load: {model: closed_vus, vus: 1, duration: 1s}
    steps: [{wait: 1s}]
";
        let draft = ok(yaml);
        assert_eq!(draft.max_request_rate, Some(rate("500/s")));
        let s = &draft.scenarios[0];
        assert_eq!(s.limits.max_variables, 128);
        assert_eq!(s.limits.max_cookies, 32);
        assert_eq!(s.max_request_rate, None);
        assert_eq!(draft.scenarios[1].max_request_rate, Some(rate("100/s")));
    }

    #[test]
    fn state_and_base_url_map_through() {
        let yaml = MINIMAL.replace("    steps", "    state: reset_each_iteration\n    steps");
        assert_eq!(
            ok(&yaml).scenarios[0].state,
            StatePolicy::ResetEachIteration
        );
        let bad = MINIMAL.replace("    steps", "    state: x\n    steps");
        fails(
            &bad,
            "scenarios[0].state",
            "expected one of persist or reset_each_iteration",
        );
        let draft = ok(&with_top("base_url: \"${env.B}\""));
        assert_eq!(
            draft.base_url,
            Some("${env.B}".parse::<Template>().unwrap())
        );
        fails(
            &with_top("base_url: \"${\""),
            "base_url",
            "expected a closing }",
        );
    }

    #[test]
    fn every_missing_required_key_is_reported() {
        let (_, errors) = build("schema_version: 1\n");
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors.iter().all(|e| e.path.is_empty()));
        assert!(errors.iter().any(|e| e.message.contains("`name`")));
        assert!(errors.iter().any(|e| e.message.contains("`scenarios`")));

        let yaml = "schema_version: 1\nname: n\nscenarios: [{}]\n";
        let (_, errors) = build(yaml);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors.iter().all(|e| e.path == "scenarios[0]"));
        for key in ["name", "load", "steps"] {
            let want = format!("missing required key `{key}`");
            assert!(errors.iter().any(|e| e.message.contains(&want)), "{key}");
        }
    }

    #[test]
    fn unknown_keys_are_reported_at_each_level() {
        fails(&with_top("x: 1"), "x", "unknown key `x`");
        let yaml = MINIMAL.replace("    steps", "    x: 1\n    steps");
        fails(&yaml, "scenarios[0].x", "unknown key `x`");
    }

    #[test]
    fn empty_lists_pass_through_compile() {
        let (draft, errors) = build("schema_version: 1\nname: n\nscenarios: []\n");
        assert!(errors.is_empty(), "{errors:?}");
        assert!(draft.is_some());
    }

    #[test]
    fn locate_follows_segments_exactly() {
        use vush_core::plan::PathSegment::{Index, Key};
        let root = super::testing::node("{a.b: {c: [1, 2]}, x: 1}");
        let NodeKind::Map(entries) = &root.kind else {
            panic!("not a map");
        };
        let ab = &entries[0].value;
        let NodeKind::Map(inner) = &ab.kind else {
            panic!("not a map");
        };
        let NodeKind::List(items) = &inner[0].value.kind else {
            panic!("not a list");
        };
        let path = |segments| PlanPath::new(segments);
        let full = path(vec![Key("a.b".into()), Key("c".into()), Index(1)]);
        assert_eq!(locate(&root, &full, false), items[1].location);
        let missing = path(vec![Key("a.b".into()), Key("nope".into()), Index(1)]);
        assert_eq!(locate(&root, &missing, false), ab.location);
        let past_end = path(vec![Key("a.b".into()), Key("c".into()), Index(5)]);
        assert_eq!(locate(&root, &past_end, false), inner[0].value.location);
        let on_map = path(vec![Key("a.b".into()), Index(0)]);
        assert_eq!(locate(&root, &on_map, false), ab.location);
        assert_eq!(locate(&root, &PlanPath::default(), false), root.location);
    }

    #[test]
    fn locate_can_point_at_the_key() {
        use vush_core::plan::PathSegment::Key;
        let root = super::testing::node("{a.b: 1}");
        let NodeKind::Map(entries) = &root.kind else {
            panic!("not a map");
        };
        let path = PlanPath::new(vec![Key("a.b".into())]);
        assert_eq!(locate(&root, &path, true), entries[0].key_location);
        assert_eq!(locate(&root, &path, false), entries[0].value.location);
    }

    #[test]
    fn locate_finds_a_cut_key() {
        use vush_core::plan::PathSegment::{Key, TruncatedKey};
        let prefix = "k".repeat(256);
        // The first key is the prefix itself, which isn't longer than it, so
        // the cut key is the second.
        let yaml = format!("{{{prefix}: 1, {prefix}kk: {{a: 2}}, {prefix}k: 3}}");
        let root = super::testing::node(&yaml);
        let NodeKind::Map(entries) = &root.kind else {
            panic!("not a map");
        };
        let NodeKind::Map(inner) = &entries[1].value.kind else {
            panic!("not a map");
        };
        let cut = PlanPath::new(vec![TruncatedKey(prefix.clone())]);
        assert_eq!(locate(&root, &cut, true), entries[1].key_location);
        assert_eq!(locate(&root, &cut, false), entries[1].value.location);
        let below = PlanPath::new(vec![TruncatedKey(prefix.clone()), Key("a".into())]);
        assert_eq!(locate(&root, &below, true), inner[0].key_location);
        assert_eq!(locate(&root, &below, false), inner[0].value.location);
        let none = PlanPath::new(vec![TruncatedKey("x".repeat(256))]);
        assert_eq!(locate(&root, &none, true), root.location);
    }

    #[test]
    fn render_path_shows_a_cut_key_like_the_whole_key() {
        use vush_core::plan::PathSegment::{Index, Key, TruncatedKey};
        // The 256-byte prefix of each key: 256 `k`s, 128 `é`s, 64 `😀`s.
        for (whole, prefix) in [
            ("k".repeat(300), "k".repeat(256)),
            ("é".repeat(1000), "é".repeat(128)),
            ("😀".repeat(100), "😀".repeat(64)),
        ] {
            let render =
                |last| render_path(&PlanPath::new(vec![Key("a\n".into()), Index(2), last]));
            let want = render(Key(whole));
            assert_eq!(render(TruncatedKey(prefix)), want);
            assert!(want.ends_with('…') && !want.ends_with("……"), "{want}");
        }
    }

    #[test]
    fn render_path_escapes_each_key() {
        use vush_core::plan::PathSegment::{Index, Key};
        let long = "k".repeat(65);
        let path = PlanPath::new(vec![Key("x\ny".into()), Index(0), Key(long)]);
        assert_eq!(render_path(&path), format!("x\\ny[0].{}…", "k".repeat(64)));
    }
}
