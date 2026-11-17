//! A plan's executors, and the build steps they share with a lone executor.

use std::path::Path;
use std::time::Duration;

use rustls::RootCertStore;
use vush_core::plan::{HttpVersion, PoolScope, WorkloadPlan};

use crate::connector::Connector;
use crate::executor::{BuildError, Executor, Scope, Transport, client};
use crate::pool::{PoolKey, ScopeView, limit, pool_key, scopes};
use crate::tls::{Trust, Verification, add_ca_file, client_config, system_roots};

/// Every executor a plan needs: one for the file-level hooks, and one per
/// scenario for its steps and hooks.
///
/// Pools are shared by each scope's `pool`:
/// - `shared`: one pool for every scope, the file level included, with the
///   same `version` and the same trust. Trust is the system's roots, the
///   system's roots plus one `tls.ca_file`, or `tls.insecure`. A `ca_file`
///   is compared as the path it resolves to, so two spellings of one file
///   don't share. Scopes whose version or trust differ never share a
///   connection, so one made without verification never serves settings
///   that verify.
/// - `scenario`: a pool of the scope's own. The file level is one more
///   scope.
/// - `vu`: none in the executor. Each session builds its own, which never
///   joins a shared pool.
///
/// Settings other than `version`, `pool` and `tls`, such as
/// `max_response_size`, are each executor's own, so they don't stop scopes
/// from sharing. Each pool's background connects are bounded by the longest
/// attempt timeout of the requests it serves, and at least by each of its
/// scopes' `http.timeout`. Sharing a pool never shares cookies or
/// credentials.
///
/// Building needs no runtime, but `send` must run inside a Tokio runtime.
pub struct Executors {
    file: Executor,
    scenarios: Vec<Executor>,
}

impl Executors {
    /// The executors of `plan`. They trust the system's root certificates,
    /// plus each scope's `tls.ca_file`, which is resolved against
    /// `test_dir`, the test file's directory. The system's roots are loaded
    /// once, and only when some scope verifies. Each `tls.ca_file` is read
    /// once per path.
    ///
    /// Building fails with [`BuildError::CaFile`], naming the first scope
    /// that reads the file, when a `tls.ca_file` can't be used, and with
    /// [`BuildError::NoTrustedRoots`] when settings that verify have no root.
    pub fn new(plan: &WorkloadPlan, test_dir: &Path) -> Result<Self, BuildError> {
        Self::of_plan(plan, test_dir, SystemRoots::Load)
    }

    /// Like [`Executors::new`], with `system` standing in for the system's
    /// roots. It never fails with [`BuildError::NoTrustedRoots`].
    pub fn with_roots(
        plan: &WorkloadPlan,
        test_dir: &Path,
        system: RootCertStore,
    ) -> Result<Self, BuildError> {
        Self::of_plan(plan, test_dir, SystemRoots::StandIn(system))
    }

    fn of_plan(
        plan: &WorkloadPlan,
        test_dir: &Path,
        system: SystemRoots,
    ) -> Result<Self, BuildError> {
        let mut executors = build(&scopes(plan), test_dir, system)?.into_iter();
        let file = executors.next().expect("the file level is the first scope");
        Ok(Self {
            file,
            scenarios: executors.collect(),
        })
    }

    /// The executor for the file-level `setup` and `teardown`.
    pub fn file(&self) -> &Executor {
        &self.file
    }

    /// The executor for the scenario at `index`, in plan order: its steps
    /// and hooks.
    pub fn scenario(&self, index: usize) -> Option<&Executor> {
        self.scenarios.get(index)
    }
}

/// Where the system's roots come from.
pub(crate) enum SystemRoots {
    /// The system's own store. Settings that verify and end up with no root
    /// fail the build with [`BuildError::NoTrustedRoots`].
    Load,
    /// A store that stands in for the system's. No settings fail for having
    /// no root.
    StandIn(RootCertStore),
}

/// One pool, before it is built.
struct PoolSpec {
    key: PoolKey,
    version: HttpVersion,
    trust: Trust,
    /// Whether each session builds its own pool, under `vu`.
    per_session: bool,
    /// The longest connect, over every scope that uses the pool.
    limit: Duration,
}

/// Builds one executor per view, in view order. Views with equal pool keys
/// share one pool, and each pool gets one TLS config and one connector.
pub(crate) fn build(
    views: &[ScopeView<'_>],
    test_dir: &Path,
    system: SystemRoots,
) -> Result<Vec<Executor>, BuildError> {
    let roots = roots_by_trust(views, test_dir, system)?;

    let mut specs: Vec<PoolSpec> = Vec::new();
    let mut pool_of = Vec::with_capacity(views.len());
    for view in views {
        let key = pool_key(view, test_dir);
        let index = match specs.iter().position(|spec| spec.key == key) {
            Some(index) => {
                specs[index].limit = specs[index].limit.max(limit(view));
                index
            }
            None => {
                specs.push(PoolSpec {
                    key,
                    version: view.settings.version,
                    trust: Trust::of(view.settings, test_dir),
                    per_session: view.settings.pool == PoolScope::Vu,
                    limit: limit(view),
                });
                specs.len() - 1
            }
        };
        pool_of.push(index);
    }

    let pools: Vec<Transport> = specs
        .into_iter()
        .map(|spec| {
            let verification = match &spec.trust {
                Trust::Insecure => Verification::Insecure,
                trust => Verification::Roots(
                    roots
                        .iter()
                        .find(|(found, _)| found == trust)
                        .map(|(_, roots)| roots.clone())
                        .expect("every trust that verifies has its roots"),
                ),
            };
            let tls = client_config(verification, spec.version);
            let connector = Connector::new(tls, spec.version, spec.limit);
            if spec.per_session {
                Transport::PerSession {
                    connector,
                    version: spec.version,
                }
            } else {
                Transport::Pooled(Box::new(client(connector, spec.version)))
            }
        })
        .collect();

    // A clone of a pooled transport shares its pool.
    Ok(views
        .iter()
        .zip(pool_of)
        .map(|(view, index)| {
            Executor::from_parts(pools[index].clone(), view.settings.max_response_size.0)
        })
        .collect())
}

/// The roots of each distinct trust of `views` that verifies, in view
/// order. The system's roots are read once, and only when some view
/// verifies. Each `tls.ca_file` is read once and added to a copy of them,
/// and its problem names the first view that reads it.
fn roots_by_trust(
    views: &[ScopeView<'_>],
    test_dir: &Path,
    system: SystemRoots,
) -> Result<Vec<(Trust, RootCertStore)>, BuildError> {
    let verifying: Vec<(Scope, Trust)> = views
        .iter()
        .map(|view| (view.at, Trust::of(view.settings, test_dir)))
        .filter(|(_, trust)| *trust != Trust::Insecure)
        .collect();
    if verifying.is_empty() {
        return Ok(Vec::new());
    }
    let (system, required) = match system {
        SystemRoots::Load => (system_roots(), true),
        SystemRoots::StandIn(roots) => (roots, false),
    };
    let mut found: Vec<(Trust, RootCertStore)> = Vec::new();
    for (at, trust) in verifying {
        if found.iter().any(|(seen, _)| *seen == trust) {
            continue;
        }
        let mut roots = system.clone();
        if let Trust::SystemPlus(path) = &trust {
            add_ca_file(&mut roots, path).map_err(|problem| BuildError::CaFile { at, problem })?;
        }
        if required && roots.is_empty() {
            return Err(BuildError::NoTrustedRoots);
        }
        found.push((trust, roots));
    }
    Ok(found)
}
