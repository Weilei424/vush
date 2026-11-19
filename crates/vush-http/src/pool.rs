//! Which scopes of a plan share a pool, and how long a pool's background
//! connects may take.
//!
//! A scope is the file level or one scenario, with its merged settings and
//! the step lists that run under them. Under `pool: shared`, scopes with the
//! same version and trust share a pool. Under `scenario` and `vu`, a scope
//! has a pool of its own.

use std::path::Path;
use std::time::Duration;

use vush_core::plan::{HttpSettings, HttpVersion, PoolScope, Step, WorkloadPlan};

use crate::executor::Scope;
use crate::tls::Trust;

/// A scope of a plan: where it is, its merged settings, and every step list
/// whose requests its executor sends.
pub(crate) struct ScopeView<'a> {
    pub at: Scope,
    pub settings: &'a HttpSettings,
    pub lists: Vec<&'a [Step]>,
}

/// Every scope of `plan`: the file level first, with its `setup` and
/// `teardown`, then each scenario in plan order, with its `setup`,
/// `teardown`, `vu_setup`, `vu_teardown` and `steps`.
pub(crate) fn scopes(plan: &WorkloadPlan) -> Vec<ScopeView<'_>> {
    let file = ScopeView {
        at: Scope::File,
        settings: plan.http(),
        lists: vec![plan.setup(), plan.teardown()],
    };
    let scenarios = plan
        .scenarios()
        .iter()
        .enumerate()
        .map(|(index, scenario)| ScopeView {
            at: Scope::Scenario(index),
            settings: &scenario.http,
            lists: vec![
                &scenario.setup,
                &scenario.teardown,
                &scenario.vu_setup,
                &scenario.vu_teardown,
                &scenario.steps,
            ],
        });
    std::iter::once(file).chain(scenarios).collect()
}

/// Which pool a scope uses. Scopes with equal keys share one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PoolKey {
    /// The pool of every `shared` scope that speaks `version` and trusts
    /// `trust`.
    Shared { version: HttpVersion, trust: Trust },
    /// The scope's own pool, under `scenario` and `vu`.
    Own(Scope),
}

/// The pool `view` uses. A `tls.ca_file` is resolved against `test_dir`, so
/// two spellings of one file make two keys.
pub(crate) fn pool_key(view: &ScopeView<'_>, test_dir: &Path) -> PoolKey {
    match view.settings.pool {
        PoolScope::Shared => PoolKey::Shared {
            version: view.settings.version,
            trust: Trust::of(view.settings, test_dir),
        },
        PoolScope::Scenario | PoolScope::Vu => PoolKey::Own(view.at),
    }
}

/// How long a background connect for `view` may take: the longest attempt
/// timeout of its requests, inside every `if` and `repeat`, and at least its
/// `http.timeout`. No hop's own deadline is later, so the limit never cuts a
/// hop short.
pub(crate) fn limit(view: &ScopeView<'_>) -> Duration {
    let mut longest = view.settings.timeout;
    // A stack of the lists left to walk, so nesting can't grow the call
    // stack.
    let mut lists = view.lists.clone();
    while let Some(list) = lists.pop() {
        for step in list {
            match step {
                Step::Request(request) => longest = longest.max(request.timeout),
                Step::Wait(_) => {}
                Step::If(step) => lists.extend([step.then.as_slice(), &step.otherwise]),
                Step::Repeat(step) => lists.push(&step.steps),
            }
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use vush_core::plan::{
        ByteSize, ClosedStop, ClosedVusLoad, Comparison, Condition, Expect, IfStep, LoadModel,
        PlanDraft, RepeatStep, RequestStep, RequestTarget, ResourceLimits, Scenario, StatePolicy,
        VarRef,
    };

    use super::*;

    /// The default `http.timeout`.
    const FLOOR: Duration = Duration::from_secs(30);
    const LONG: Duration = Duration::from_secs(45);

    fn request(timeout: Duration) -> Step {
        Step::Request(Box::new(RequestStep {
            name: None,
            method: "GET".parse().unwrap(),
            target: RequestTarget::Url("http://127.0.0.1/".parse().unwrap()),
            query: vec![],
            headers: vec![],
            body: None,
            timeout,
            required: true,
            retry: None,
            expect: Expect::default(),
            extract: vec![],
        }))
    }

    fn wait() -> Step {
        Step::Wait(Duration::from_secs(1))
    }

    /// A condition that reads only a built-in value.
    fn when() -> Condition {
        Condition::Compare {
            var: VarRef::VuId,
            op: Comparison::Exists(true),
        }
    }

    fn view<'a>(at: Scope, settings: &'a HttpSettings, lists: Vec<&'a [Step]>) -> ScopeView<'a> {
        ScopeView {
            at,
            settings,
            lists,
        }
    }

    fn key(at: Scope, settings: &HttpSettings) -> PoolKey {
        pool_key(&view(at, settings, vec![]), Path::new("/t"))
    }

    /// The default settings, changed by `change`.
    fn with(change: impl FnOnce(&mut HttpSettings)) -> HttpSettings {
        let mut settings = HttpSettings::default();
        change(&mut settings);
        settings
    }

    #[test]
    fn pool_keys_follow_the_profile() {
        let (a, b) = (Scope::Scenario(0), Scope::Scenario(1));
        let base = HttpSettings::default();
        assert_eq!(key(a, &base), key(b, &base));
        assert_eq!(key(Scope::File, &base), key(b, &base));

        let differ = [
            ("version", with(|s| s.version = HttpVersion::Http1)),
            ("insecure", with(|s| s.tls.insecure = true)),
            (
                "ca_file",
                with(|s| s.tls.ca_file = Some("a.pem".to_owned())),
            ),
        ];
        for (what, changed) in &differ {
            assert_ne!(key(a, &base), key(b, changed), "{what}");
        }
        let other_ca_file = with(|s| s.tls.ca_file = Some("b.pem".to_owned()));
        assert_ne!(key(a, &differ[2].1), key(b, &other_ca_file));

        // Each spelling of one file is a key of its own, even those that
        // `Path` compares as equal.
        let spellings = ["a.pem", "./a.pem", "a.pem/", ".//a.pem"];
        for (i, one) in spellings.iter().enumerate() {
            for other in &spellings[i + 1..] {
                let one_key = key(a, &with(|s| s.tls.ca_file = Some((*one).to_owned())));
                let other_key = key(b, &with(|s| s.tls.ca_file = Some((*other).to_owned())));
                assert_ne!(one_key, other_key, "{one} and {other}");
            }
        }

        let same = [
            ("timeout", with(|s| s.timeout = Duration::from_secs(1))),
            ("redirects", with(|s| s.redirects = 0)),
            ("cookies", with(|s| s.cookies = false)),
            (
                "max_response_size",
                with(|s| s.max_response_size = ByteSize(1)),
            ),
        ];
        for (what, changed) in &same {
            assert_eq!(key(a, &base), key(b, changed), "{what}");
        }

        let insecure = with(|s| s.tls.insecure = true);
        let insecure_with_ca_file = with(|s| {
            s.tls.insecure = true;
            s.tls.ca_file = Some("a.pem".to_owned());
        });
        assert_eq!(key(a, &insecure), key(b, &insecure_with_ca_file));

        for pool in [PoolScope::Scenario, PoolScope::Vu] {
            let own = with(|s| s.pool = pool);
            assert_eq!(key(a, &own), PoolKey::Own(a), "{pool:?}");
            for other in [Scope::File, b] {
                assert_ne!(key(a, &own), key(other, &own), "{pool:?}");
                assert_ne!(key(a, &own), key(other, &base), "{pool:?}");
            }
        }
    }

    #[test]
    fn the_limit_is_the_longest_request_timeout() {
        let settings = HttpSettings::default();
        assert_eq!(settings.timeout, FLOOR);
        let short = [request(Duration::from_secs(10))];
        assert_eq!(limit(&view(Scope::File, &settings, vec![&short])), FLOOR);

        let in_otherwise = [Step::If(IfStep {
            when: when(),
            then: vec![wait()],
            otherwise: vec![Step::Repeat(RepeatStep {
                count: NonZeroU32::MIN,
                deadline: Duration::from_secs(60),
                steps: vec![wait(), request(LONG)],
            })],
        })];
        let lists: Vec<&[Step]> = vec![&short, &in_otherwise];
        assert_eq!(limit(&view(Scope::File, &settings, lists)), LONG);

        let in_then = [Step::If(IfStep {
            when: when(),
            then: vec![request(LONG)],
            otherwise: vec![],
        })];
        assert_eq!(limit(&view(Scope::File, &settings, vec![&in_then])), LONG);

        let no_requests = [wait()];
        assert_eq!(limit(&view(Scope::File, &settings, vec![])), FLOOR);
        assert_eq!(
            limit(&view(Scope::File, &settings, vec![&no_requests])),
            FLOOR
        );
    }

    /// The list of a plan that holds its one long request.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum List {
        FileSetup,
        FileTeardown,
        Setup,
        Teardown,
        VuSetup,
        VuTeardown,
        Steps,
    }

    /// A plan with one scenario, whose only long request is in `list`.
    /// Every other list is empty, except the scenario's steps, which wait.
    fn plan_with_long_request_in(list: List) -> WorkloadPlan {
        let mut scenario = Scenario {
            name: "s0".to_owned(),
            load: LoadModel::ClosedVus(ClosedVusLoad::Fixed {
                vus: NonZeroU32::MIN,
                stop: ClosedStop::IterationsPerVu(NonZeroU64::MIN),
            }),
            http: HttpSettings::default(),
            limits: ResourceLimits::default(),
            max_request_rate: None,
            state: StatePolicy::Persist,
            setup: vec![],
            teardown: vec![],
            vu_setup: vec![],
            vu_teardown: vec![],
            steps: vec![wait()],
        };
        let mut draft = PlanDraft {
            name: "test".to_owned(),
            base_url: None,
            http: HttpSettings::default(),
            limits: ResourceLimits::default(),
            max_request_rate: None,
            setup: vec![],
            teardown: vec![],
            thresholds: vec![],
            scenarios: vec![],
        };
        let steps = match list {
            List::FileSetup => &mut draft.setup,
            List::FileTeardown => &mut draft.teardown,
            List::Setup => &mut scenario.setup,
            List::Teardown => &mut scenario.teardown,
            List::VuSetup => &mut scenario.vu_setup,
            List::VuTeardown => &mut scenario.vu_teardown,
            List::Steps => &mut scenario.steps,
        };
        steps.push(request(LONG));
        draft.scenarios.push(scenario);
        WorkloadPlan::new(draft).unwrap()
    }

    #[test]
    fn every_hook_counts_toward_the_limit() {
        for list in [
            List::FileSetup,
            List::FileTeardown,
            List::Setup,
            List::Teardown,
            List::VuSetup,
            List::VuTeardown,
            List::Steps,
        ] {
            let plan = plan_with_long_request_in(list);
            let views = scopes(&plan);
            let at: Vec<Scope> = views.iter().map(|view| view.at).collect();
            assert_eq!(at, [Scope::File, Scope::Scenario(0)], "{list:?}");
            let file_level = matches!(list, List::FileSetup | List::FileTeardown);
            let (long, floor) = if file_level { (0, 1) } else { (1, 0) };
            assert_eq!(limit(&views[long]), LONG, "{list:?}");
            assert_eq!(limit(&views[floor]), FLOOR, "{list:?}");
        }
    }
}
