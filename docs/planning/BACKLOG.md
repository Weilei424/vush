# Backlog — Vush

> **Status:** MVP planning baseline, 2026-11-01. Phase 0 complete 2026-11-01 (CI green on all four targets); Phase 1 in progress; Phases 2–8 remain baseline planning. `IMPLEMENTATION_PLAN.md` defines phase goals and acceptance criteria; `ARCHITECTURE_NOTES.md` is the architecture reference. This is a phase/task checklist, **not** a substitute for phase-specific design specs/plans.

## Status legend

- `[ ]` Not started
- `[x]` Complete, verified
- `[~]` In progress (replace with actual state in normal project workflow; GitHub Markdown may not render this as a checkbox)

## MVP acceptance contract

- **Vush** (`vush` CLI): Rust async, performance-first single-host HTTP load/smoke runner.
- YAML/JSON -> one typed plan; Linux, macOS, Docker CLI.
- Stateful workflows, extractions, cookies, assertions, think time, bounded control flow and scoped hooks.
- Fixed/ramping VUs, iteration arrival rates, and distinct request-start pacing with global/per-scenario controls.
- Bounded scheduling, dropped/expired work accounting and sampled nonblocking diagnostic events; no DLQ replay.
- Drain accepted non-expired queue and await timed in-flight work on normal test end.
- Human summary, stable JSON export, thresholds, CI exit codes; generator-vs-SUT metrics.
- k6-equivalent benchmarks and documented absolute measurement baselines; 100K sleeping VUs is aspirational.

## Phase 0 Execution Checklist — Repository foundation

> Spec: `docs/superpowers/specs/2026-11-01-phase-0-repository-foundation-design.md` · Plan: `docs/superpowers/plans/2026-11-01-phase-0-repository-foundation.md` (local only; `docs/superpowers/` is gitignored).

- [x] P0.1 Inspect actual repository, current code/tests, root agent instructions and existing planning doc paths.
  - Findings (2026-11-01): single root crate `vush` (edition 2024, no dependencies); `src/main.rs` is the `cargo new` hello-world stub; no tests, CI, `examples/` or `crates/` yet. `.gitignore` ignores `/target`. Toolchain rustc/cargo 1.96. `cargo fmt --check`, `cargo clippy --all-targets -D warnings` and `cargo test` pass on the stub. Root `CLAUDE.md` / `AGENTS.md` were copied from another project and have been rewritten for Vush. Project skill: `.claude/skills/plan-on-step/`.
- [x] P0.2 Resolve canonical `BACKLOG.md`, `IMPLEMENTATION_PLAN.md`, `ARCHITECTURE_NOTES.md` without duplicate files.
  - Resolved: all three live in `docs/planning/` (none at root, so no duplicates). Phase specs go in `docs/superpowers/specs/` and plans in `docs/superpowers/plans/` (neither directory exists yet; create them on first use).
- [x] P0.3 Initialize/build Rust workspace and `vush` CLI binary stub (design approved 2026-11-01; plan Tasks 1–3).
  - [x] Pin toolchain in `rust-toolchain.toml` (`1.99.1`, `rustfmt` + `clippy`); add `.gitattributes` (`* text=auto eol=lf`); clear the executable bit on `docs/planning/*.md`.
  - [x] Convert to a virtual workspace: resolver `"3"`, shared package fields (`0.1.0`, edition 2024, Apache-2.0, repository, `publish = false`), workspace `clap` 4.6, `unsafe_code = "forbid"`; remove root `src/`.
  - [x] Add crates `crates/vush` (binary), `crates/vush-core` and `crates/vush-http` (doc-only libraries, no inter-crate dependencies yet).
  - [x] Implement the clap CLI stub: `--help`/`-h` and `--version`/`-V` exit 0; no args prints help to stderr and exits 2; unknown arguments exit 2.
  - [x] Add 6 integration tests in `crates/vush/tests/cli.rs` (help, version, short flags, no args, unknown flag, `run` rejected before Phase 1).
- [x] P0.4 Set up `cargo fmt`, `clippy`, tests and CI (plan Task 4).
  - [x] Add `.github/workflows/ci.yml`: an `fmt` job, plus a clippy (`-D warnings`) and test (`--all-targets`, `--doc`, `--locked`) matrix on `ubuntu-24.04`, `ubuntu-24.04-arm`, `macos-15` and `macos-15-intel`.
  - [x] actionlint passes locally (via Docker).
- [x] P0.5 Add contributor README and root `AGENTS.md`/`CLAUDE.md`/`GEMINI.md` if missing and approved (plan Tasks 5–6).
  - [x] Root `CLAUDE.md` and `AGENTS.md` rewritten for Vush (2026-11-01).
  - [x] README: Development (commands, single-test example), supported targets table, optional local arm64 Docker check (verified: passed under emulation in 84s), MVP non-goals, compatibility policy.
  - [x] Refresh local `CLAUDE.md`/`AGENTS.md` with the actual layout, commands, toolchain and CI.
- [x] P0.6 Verify clean commands and `vush --help` / `vush --version`; record actual paths/commands in phase plan (plan Task 6).
  - [x] All four check commands pass locally on 1.99.1; every tracked file stages as `100644`; agent files and `docs/superpowers/` stay ignored.
  - [x] First CI run green on all four runners plus `fmt` (run 41338362501 on `13e08f1`, 2026-11-01).

## Phase 1 Execution Checklist — Input model and validation

> P1.1 spec: `docs/superpowers/specs/2026-11-02-phase-1-1-config-shape-and-plan-design.md` · Plan: `docs/superpowers/plans/2026-11-02-phase-1-1-config-shape-and-plan.md` (local only; `docs/superpowers/` is gitignored).

- [~] P1.1 Agree versioned YAML/JSON configuration shape and typed immutable workload plan (design approved 2026-11-02).
  - [x] Value types in `vush_core::plan`: durations, rates, sizes and percentages, parsed from text with exact-value and rejection tables (plan Task 1).
  - [x] `Template` and `VarRef`: `${...}` parsing with `env`, `global`, `scenario`, `vu` scopes and `$${` escaping (plan Task 2).
  - [x] `JsonPath`: the `$.a.b[0].c` subset (plan Task 3).
  - [x] Plan data model: HTTP settings and limits with schema defaults, the three load models, steps, comparisons and thresholds (plan Task 4).
  - [x] `WorkloadPlan::new` as the only constructor: reports every structural error with a file-key path; the plan is read-only and `Send + Sync` (plan Task 5).
  - [x] `docs/config-schema.md`: schema v1 reference with one complete YAML + JSON example, built through the public API in a test (plan Task 6).
  - [x] Replace `ARCHITECTURE_NOTES.md` §5 with a pointer to the schema doc; refresh local `CLAUDE.md`/`AGENTS.md` (plan Tasks 6–7).
  - [x] All four check commands pass and `vush-core` still has no dependencies (plan Task 7).
  - [ ] CI green on all four runners plus `fmt` after the user pushes (plan Task 7).
- [ ] P1.2 Parse equivalent YAML/JSON into the same plan; select maintained YAML library. Creates `crates/vush-config` (decided 2026-11-02).
  - Numbers follow the `JsonNumber` variant rule in `value.rs`. Integers outside the range `i64::MIN` to `u64::MAX` are errors, never stored as `Float`.
  - Duplicate map keys are errors in both formats.
  - YAML number forms such as `010`, `0x10` and `.inf` are decided with the library.
- [ ] P1.3 Validate durations, rates, load modes, loops, bounds, variable scopes and unsupported fields.
  - A nesting depth limit covering steps, `when` conditions and `json` values, enforced in `vush-config` before a draft is built.
  - An upper bound on durations.
  - `extract` names must match the variable-name pattern and avoid the reserved words (`env`, `global`, `scenario`, `vu`).
  - Scope rules include `extract` in `teardown`.
  - `${` in a request `name` is an error.
  - A `retry` on any method other than GET, HEAD, PUT, DELETE, OPTIONS and TRACE needs `allow_non_idempotent`.
- [ ] P1.4 Implement `vush validate` and meaningful config diagnostics.
  - Diagnostics stay bounded and don't echo text beyond `ValueError`'s bounded span. `ValueError`'s `Debug` output still prints all of `input`, so don't surface it through `{:?}`, `unwrap` or `expect` on file input.
  - `dry-run` never prints the plan's `Debug` output, which holds literal header and body text.
  - The README links `docs/config-schema.md` once `vush validate` exists.
- [ ] P1.5 Add positive/negative fixtures, equivalent-plan and CLI exit tests. Fixtures include the `docs/config-schema.md` example so the doc can't drift. Fixtures also check the documented defaults tables against the `Default` impls and constants.

## Phase 2 Execution Checklist — HTTP executor

- [ ] P2.1 Implement protocol-neutral request/observation/error boundary.
- [ ] P2.2 Support HTTP1.1/2 over HTTP/HTTPS; redirects, methods, headers, query and bodies.
- [ ] P2.3 Support pooling configurations, TLS, explicit timeouts and opt-in retries. Define `pool: shared` when scenarios differ in TLS or HTTP version; a connection opened with verification off must never serve a scenario that kept it on.
- [ ] P2.4 Guarantee credentials/cookies cannot leak through shared pooled transport.
- [ ] P2.5 Implement bounded body capture and safe error diagnostics.
- [ ] P2.6 Add local test-server integration tests for success, failure, delay, redirect, oversized body, TLS and reuse.

## Phase 3 Execution Checklist — Scenario interpreter

- [ ] P3.1 Implement ordered step execution and fail-fast per-iteration policy.
- [ ] P3.2 Add per-VU state, JSON/header extraction, interpolation and cookie/token propagation.
- [ ] P3.3 Add status/content/JSON assertions and distinct failure categories.
- [ ] P3.4 Add think time, structured `if/else` and bounded loops.
- [ ] P3.5 Implement global/per-scenario declarative setup/teardown and define their failure policies.
- [ ] P3.6 Enforce body, variable, loop and hook bounds; test auth-flow and failing-flow cases.

## Phase 4 Execution Checklist — Stateful VUs and closed scheduling

- [ ] P4.1 Implement VU lifecycle/ownership; one iteration at a time; persistent state by default.
- [ ] P4.2 Implement fixed and ramping VU schedules; avoid one OS thread per VU.
- [ ] P4.3 Add configurable per-iteration state reset and shared/scenario/VU connection scope.
- [ ] P4.4 Implement per-VU setup/teardown and cleanup on retirement/scale-down.
- [ ] P4.5 Verify isolation under concurrency, memory bounds and closed-loop semantics.

## Phase 5 Execution Checklist — Open/load control and overload

- [ ] P5.1 Approve explicit request-start rate semantics for workflows, global/per-scenario budgets and retry accounting.
- [ ] P5.2 Implement fixed/ramping scenario-iteration arrival, independent of completion.
- [ ] P5.3 Implement request-start pacing; report target vs achieved rate and scheduler lateness.
- [ ] P5.4 Implement bounded queue/active budget, drop-newest overload, configurable expiry and detailed reason counters.
- [ ] P5.5 Implement sampled bounded drop-event sink with separate event-loss counter; **no automatic replay**.
- [ ] P5.6 Stop arrivals on deadline, drain accepted non-expired queue, finish/timeout in-flight work, run teardown.
- [ ] P5.7 Verify queue conservation, fairness, slow-target behavior, deadlines and abnormal shutdown.

## Phase 6 Execution Checklist — Metrics and reporting

- [ ] P6.1 Document metric names/units/windows; bounded histograms and label cardinality.
- [ ] P6.2 Aggregate request/iteration time, scheduler/queue delay, achieved rates, errors, drop reasons and in-flight counts.
- [ ] P6.3 Add optional sampled detailed events with redaction and finite capacity.
- [ ] P6.4 Add assertions, performance thresholds, dropped-work thresholds and pass/fail policy.
- [ ] P6.5 Add human-readable summary, versioned JSON output and deterministic exit-code mapping.
- [ ] P6.6 Verify aggregated conservation, percentiles, secret redaction and active-vs-drain throughput accounting.

## Phase 7 Execution Checklist — Benchmarking and tuning

- [ ] P7.1 Implement reproducible controlled-target benchmark harness and reference workload fixtures.
- [ ] P7.2 Compare to k6 with equivalent request loads, connections, protocol, hardware and warmup.
- [ ] P7.3 Measure generator throughput, per-VU CPU/RSS, startup, latency accuracy and queue lag under saturation.
- [ ] P7.4 Benchmark sleeping VUs at multiple scales, with 100K as aspirational if hardware allows.
- [ ] P7.5 Profile and optimize measured hotspots with correctness/performance regression checks.
- [ ] P7.6 Publish baseline conditions, numerical budgets and known limits, without unsupported superiority claims.

## Phase 8 Execution Checklist — MVP release

- [ ] P8.1 Document config schema, flow and hook semantics, three load models, timeouts, limits and threshold behavior.
- [ ] P8.2 Add Linux/macOS release builds, Linux Docker packaging and CI examples.
- [ ] P8.3 Validate graceful signal behavior, secret redaction, container permissions and errors.
- [ ] P8.4 Run full cross-platform smoke/e2e/benchmark regression and resolve release blockers.
- [ ] P8.5 Publish MVP release notes, supported platform matrix and result JSON compatibility policy.

## Planning / agent handoff checklist (repeat per phase)

- [ ] Confirm exact `plan on phase N` / `plan on step N` trigger before invoking supplied planning skill.
- [ ] Inspect real repo root instructions, source, tests and discovered planning docs.
- [ ] Present 2–3 phase-scoped design approaches with recommendation; obtain approval.
- [ ] Create/update phase-specific spec in repo's spec directory and task plan in plan directory, with exact paths and tests.
- [ ] Update this phase checklist; update architecture/roadmap only for material changes.
- [ ] **Stop before implementation** and request separate execution confirmation.

## Known architectural questions to settle when reached (not blockers to overall roadmap)

- [x] Phase 0: minimal Cargo workspace/crate boundaries and actual repository conventions. Decided 2026-11-01: `crates/vush` (bin), `crates/vush-core`, `crates/vush-http`; toolchain pinned in `rust-toolchain.toml`; CI on Linux x86_64/arm64 and macOS arm64/x86_64.
- [ ] Phase 1: exact source-location validation behavior. Schema names and provisional default limits decided 2026-11-02 (P1.1).
- [ ] Phase 2: where the protocol-neutral executor trait lives (e.g. trait in `vush-core`, implemented by `vush-http`) so crate dependencies stay acyclic. The plan compiler's home is decided: `crates/vush-config` (2026-11-02, P1.1), so `crates/vush` doesn't need a library target for it.
- [ ] Phase 2: maintained HTTP/TLS/YAML (where applicable) dependencies and retry/pooling implementation.
- [ ] Phases 3–4: hook failure/cleanup precedence, state reset semantics, VU worker ownership. Also whether `all` and `any` stop at the first decisive condition (the schema example relies on it), whether `vu.id` and `vu.iteration` start at 0 or 1, what exceeding `max_response_size` or `deadline` does, and how `exists` treats JSON `null`.
- [ ] Phase 5: hard cap vs target request-rate, burst/fairness policy, retry attempt rate, scheduling tick precision, emergency signals.
- [ ] Phase 6: percentile algorithm, metrics schema and exit-code precedence, default threshold behavior.
- [ ] Phase 7: realistic measured numerical performance SLOs and benchmark controls.

## Backlog / future TODO — post-MVP

### Input languages and test authoring

- [ ] F1. JavaScript/TypeScript plan-generation adapter (no runtime interpreter on Rust hot path by default).
- [ ] F2. Python plan-generation adapter, with runtime/sandbox/security policy.
- [ ] F3. Explicit opt-in dynamic script hooks, isolated and benchmarked as slower path.
- [ ] F4. Human-readable, natural-language-inspired declarative DSL; define grammar/AST **later**, based on usage research.
- [ ] F5. HAR/Postman collection import and reusable templates.

### Protocols, plugins and distributed scale

- [ ] F6. WebSocket and gRPC adapters.
- [ ] F7. TCP/UDP protocol support.
- [ ] F8. Extensible custom protocol handler/plugin API, including scripting handlers and safety limits.
- [ ] F9. Distributed runner, centralized coordination and merged metrics with clock synchronization strategy.

### Product and operations

- [ ] F10. Windows support and packaging.
- [ ] F11. Prometheus/OpenTelemetry exporters and optional dashboard.
- [ ] F12. Additional CI/CD integrations and result comparison/history tools.
- [ ] F13. Optional bounded persistent rejection-event archives for offline debugging; never automatically replay rejected traffic.
- [ ] F14. Continuous performance regression tracking and reproducibility reports.
- [ ] F15. Advanced scenario composability/data parameterization after validating core semantics.
