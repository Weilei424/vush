# Implementation Plan — Vush

> **Status:** Baseline roadmap (2026-11-01). Phase 0 is complete (designed, approved, implemented; CI green 2026-11-01); Phase 1 is complete (designed, approved, implemented; CI green 2026-11-11); Phases 2–8 are not yet planned in detail and remain baseline scope. Planning is intentionally broken into explicit phases so the coding agent can invoke `plan on phase N`, read repo context, design that phase, create a phase-specific spec/plan, obtain approval, and then request separate implementation confirmation.
>
> Architecture source: `ARCHITECTURE_NOTES.md`. Execution checklist: `BACKLOG.md`. Suggested canonical location: `docs/planning/`. If a real repo already has these docs at root or another discovered canonical location, use the existing paths; do not create duplicates.

## 1. Overview

Deliver **Vush**, an efficient Rust HTTP smoke/load-testing CLI (`vush`) for Linux, macOS, and Docker. Start with YAML/JSON configuration and an immutable typed workload plan, add a reliable stateful HTTP scenario executor, then layer on closed/open/request-paced scheduling, metrics, thresholds, and finally reproducible benchmark and release validation. All additional languages, protocols and distributed capabilities are **post-MVP**.

### Delivery strategy

- **Architecture-first, phase-sized detail.** Freeze critical semantics; defer algorithms, exact traits, config grammar, crates and library picks to the phase that implements them.
- **Vertical slices.** Each phase must produce a demonstrable CLI/testable behavior, not only an abstraction.
- **Correctness before optimization.** Baseline functionality must work under small load before stress/performance tuning.
- **Measured performance.** Track scheduler lag, generator capacity and memory. Do not conflate server overload with load-generator saturation.
- **Planning-only handoff.** `plan on phase N` creates/updates phase-specific specs and plans and stops before code, exactly as the provided skill requires.

### Source tree (crate layout decided in Phase 0 and P1.1, examples/ in P1.5; other paths still proposed)

```text
Cargo.toml                 # virtual workspace manifest
rust-toolchain.toml
.github/workflows/ci.yml
crates/vush/src/...        # binary: CLI, reports
crates/vush-config/src/... # YAML/JSON reading, defaults, file:line errors, builds the plan (from P1.2)
crates/vush-core/src/...   # workload plan, scheduler, scenario runner
crates/vush-http/src/...   # HTTP ProtocolExecutor
crates/*/tests/...         # integration tests live with their crate (e.g. crates/vush/tests/cli.rs)
examples/{smoke,flow,rate}.{yaml,json}  # sample tests in both formats (from P1.5)
benches/ or benchmarks/...
docs/config-schema.md      # schema v1 reference: the YAML/JSON test file format
docs/planning/{BACKLOG,IMPLEMENTATION_PLAN,ARCHITECTURE_NOTES}.md
docs/superpowers/specs/    # local only (gitignored)
docs/superpowers/plans/    # local only (gitignored)
```

A root `AGENTS.md`, `CLAUDE.md`, or `GEMINI.md` is required by the user's planning skill. The agent must discover one from the actual repository; do not overwrite existing instructions. Exact source paths below are candidate paths, not assertions of existing files.

## 2. Phases and acceptance criteria

### Phase 0 — Repository foundation and planning alignment

**Goal:** Make the repository buildable and provide reliable contributor/agent workflows.

**Deliverables:** Cargo workspace (`crates/vush` binary, `crates/vush-core`, `crates/vush-http`); `vush` CLI stub with help/version; toolchain pinned in `rust-toolchain.toml`; GitHub Actions CI running natively on Linux x86_64/arm64 and macOS Apple Silicon/Intel; formatting, lint and test commands; README development docs; root agent-instructions files; planning docs resolved at `docs/planning/` without duplicates.

**Tasks:**
1. Inspect repository, instructions, existing docs, code and tests; pick canonical planning paths as prescribed by `plan-on-step`. *(done)*
2. Pin the toolchain, add repo hygiene (LF line endings via `.gitattributes`, no stray executable bits), and convert to a virtual workspace with the three crates. Libraries hold only crate docs until their phases.
3. Implement the clap `vush --help` / `--version` stub with integration tests, and set up `cargo fmt` / `clippy -D warnings` / `test` plus the four-runner CI matrix.
4. Add README sections: development workflow, supported targets, non-goals and compatibility policy.

**Success criteria:** CI green on all four runners; help/version work; no load execution expected yet; architecture docs reference the decided crates and targets.

**Verification:** `cargo fmt --all --check`; `cargo clippy --workspace --all-targets --locked -- -D warnings`; `cargo test --workspace --all-targets --locked`; `cargo test --workspace --doc --locked`; `cargo run -p vush -- --help`; first CI run green after push.

### Phase 1 — YAML/JSON plan compiler and CLI validation

**Goal:** Convert declarative files into the same validated, immutable typed workload representation.

**Deliverables:** Schema v1 reference (`docs/config-schema.md`), typed plan in `vush-core`, `crates/vush-config` with YAML/JSON input adapters and schema/semantic validation, a `validate` CLI command, example configs. `dry-run` moved to the phase that adds `vush run`, because it also needs run-time checks such as `${env.NAME}` being set and `tls.ca_file` being readable (decided 2026-11-08).

**Tasks:**
1. Design a **single schema** with explicit load modes, scenario steps, hooks, HTTP, limits and thresholds; mark unsupported features clearly.
2. Read both formats into one located value tree, then compile it with checked defaults, helpful source-aware error messages, unknown-field handling and a schema-version guard.
3. Validate resource limits, bounded loops, variable references, schedule incompatibilities, nonzero capacities and duration formats.
4. Compile to immutable plan; add YAML-vs-JSON equivalent-plan tests, malformed config fixture tests and versioning docs.

**Success criteria:** Valid equivalent YAML/JSON produce semantically equal plans; invalid configs fail before network traffic with actionable diagnostics; `vush validate` works.

**Verification:** Unit/property-style tests for schema and validation; CLI integration tests using valid/invalid fixtures; full workspace checks.

### Phase 2 — HTTP transport and request observation

**Goal:** Implement reliable async HTTP execution isolated behind a protocol-agnostic interface.

**Deliverables:** HTTP1/HTTP2 over HTTPS, request options, response observations, connection reuse, redirect/timeout policies, opt-in retries, bounded response capture.

**Tasks:**
1. Define protocol request/observation/error interface; separate physical HTTP attempt from logical scenario step.
2. Implement HTTP methods, path/query/body/headers/TLS, HTTP2 and configurable client/pool scopes, default shared pool.
3. Make per-request credentials/cookies explicit; never globally share per-VU secrets through the transport pool.
4. Support redirects, finite timeouts, opt-in retry policy, body-size limit and safe diagnostics.
5. Exercise an in-process local HTTP test server with delayed, chunked, oversized, redirecting, TLS/HTTP2 and error responses.

**Success criteria:** Correct request and response behavior; keepalive/pooling observable in fixtures; no accidental state leakage; timeouts/oversize/retry behavior verified; no external service needed for tests.

**Verification:** HTTP integration tests; fake-server behavioral tests; workspace checks.

### Phase 3 — Stateful scenario interpreter and declarative hooks

**Goal:** Run rich, bounded scenario flows sequentially on an isolated VU state.

**Deliverables:** Request/wait/if/else/bounded-repeat nodes, JSON/header extraction, interpolation, response assertions, cookie/token propagation, scoped setup/teardown semantics and tests.

**Tasks:**
1. Implement typed scenario step runner; introduce per-VU variables/cookies and read-only global/scenario setup snapshots.
2. Extract JSON fields/headers with explicit missing-field policy; interpolate safely into headers, URLs and bodies.
3. Implement fail-fast iteration behavior on required step failure; separately classify transport failures and assertion failures.
4. Add structured conditional comparisons, bounded loops, think time, per-variable/body memory caps.
5. Define and implement declarative global/scenario hooks now; wire per-VU hooks to VU lifecycle in Phase 4; lock failure/teardown policies in the phase spec.
6. Add login -> profile -> conditional cart fixture and failure/cleanup coverage.

**Success criteria:** End-to-end flow works locally with extracted auth token; no cross-VU variable/cookie leakage; failed step stops only that iteration; loops and hooks terminate within limits.

**Verification:** Deterministic step-state tests, state-isolation tests, local fixture e2e, workspace checks.

### Phase 4 — VU lifecycle, closed-model scheduling, state/pool isolation

**Goal:** Execute many VUs concurrently with bounded resource use and correct state persistence.

**Deliverables:** Tokio-based VU lifecycle, fixed/ramping VUs, per-VU hook lifecycle, persistent/resettable state, shared/per-scenario/per-VU pool policy, control/stop pathways.

**Tasks:**
1. Implement VU ownership model with one iteration at a time per VU; private state persists by default.
2. Implement closed-loop fixed/ramping VU coordinator with finite active task/concurrency bounds.
3. Support state reset and HTTP client isolation configuration; verify shared transport does not share cookie jars.
4. Finish per-VU setup/teardown and failures; ensure VUs retire cleanly on scale-down.
5. Establish basic counters/latency collection hooks used later by reporters.

**Success criteria:** Repeatable multi-VU tests; isolation verified under concurrency; VUs can sleep without consuming OS threads each; cleanup works during ramp-down.

**Verification:** Controlled load against fixture, state persistence/isolation tests, ramp timing tests, memory sanity check, workspace checks.

### Phase 5 — Open iteration arrival, request-start rate and overload lifecycle

**Goal:** Implement all MVP workload control models with unambiguous semantics and safe overload behavior.

**Deliverables:** Fixed/ramped iteration arrival; explicit request-start pacing distinct from iterations; global/per-scenario rate controls; bounded admission and active VU budgets; expiry; nonblocking sampled dropped-work diagnostics; normal drain behavior.

**Tasks:**
1. Define precise **request-rate** driver contract (demand source, target vs hard-cap option, actual physical attempt accounting, burst bounds, scenario fairness); approve phase spec before coding.
2. Implement timestamp-based, open iteration-arrival scheduling independent of iteration completion; reuse available persistent VUs without overlapping iterations.
3. Implement physical request-start pacing for multi-step flows with global/per-scenario budgets and clear target-vs-achieved reporting.
4. Add bounded queues/permits, drop-newest on overflow, configurable stale-queue expiry and authoritative counters for each reason.
5. Add separate bounded sampled drop-event sink; if full, count lost diagnostics, never block hot path or replay dropped work.
6. On active duration end, drain accepted non-expired work and wait for all in-flight steps to complete/timeout; distinguish normal stop from signal/hard-stop; validate invariants under forced overload.

**Success criteria:** Accurate closed/open/request-rate semantics; scheduled vs started timing observable; stable memory under overload; all work accounted; rejected work never replayed; drain starts no new arrivals.

**Verification:** Injected/simulated clock scheduler tests, bounded-queue saturation tests, rate-fairness tests, forced slow-target tests, shutdown/drain e2e, workspace checks.

### Phase 6 — Metrics, assertions, thresholds and reporters

**Goal:** Make test outcomes trustworthy, comparable and CI-ready without excessive instrumentation overhead.

**Deliverables:** Bounded aggregate collector with p50/p90/p95/p99, request and iteration metrics, JSON schema/version, optional detailed request records, threshold evaluation, readable CLI summary and deterministic exit mapping.

**Tasks:**
1. Define metric names, units, dimensionality, histogram accuracy and time windows; keep labels bounded.
2. Aggregate HTTP duration, iteration time, queue wait, scheduling delay, active-window RPS/IPS, errors, drops and detailed event loss.
3. Add optional asynchronous bounded request recording with redaction, sampling and failure counters.
4. Implement checks, thresholds (status/content, latency percentiles, error rate, throughput, drops) and JSON output versioning.
5. Define exit code precedence for config errors, generator failures, threshold/assertion failures and clean pass; test CI behavior.
6. Add cross-check tests verifying aggregate conservation and that drain time is not accidentally folded into active-window throughput.

**Success criteria:** Correct low-volume golden JSON tests; large-volume aggregate totals match fixture; p95 and error threshold tests reliable; no secret leakage; reporting remains bounded under load.

**Verification:** Golden-output fixtures, deterministic metric aggregation tests, CLI process exit assertions, workspace checks.

### Phase 7 — Benchmarking, profiling and performance acceptance

**Goal:** Demonstrate generator correctness and quantify performance rather than relying on language reputation.

**Deliverables:** Reproducible benchmark harness, k6-comparable workloads, profiling notes, measured baseline and justified optimizations, high-VU idle/sleep test where hardware allows.

**Tasks:**
1. Create controlled local or LAN target capable of recording actual arrival times, statuses and bytes, avoiding network/target saturation as an uncontrolled variable.
2. Benchmark request throughput, peak/RSS memory and CPU per VU, fast startup, scheduler lag, dropped arrivals and idle/sleep VUs at multiple scales.
3. Define equivalence with k6: same hardware, target, TLS/protocol mode, connection reuse, warmup, concurrency, payload, duration and reporting definitions.
4. Investigate bottlenecks (allocations, pooled clients, task overhead, queue contention, metrics); optimize **only** with measurements and regression tests.
5. Publish conditions, confidence/variance and hardware limits. Attempt 100K mostly sleeping VUs as aspirational, not as a universal acceptance prerequisite.

**Success criteria:** Reproducible benchmark procedure and baseline report, no major correctness regressions under stress, performance tradeoffs documented, numerical budgets derived from evidence.

**Verification:** Benchmark harness commands and statistical comparison steps documented in repo; run smoke/regression suite before and after optimization.

### Phase 8 — MVP release hardening and packaging

**Goal:** Make the tool safe to run, easy to install and predictable in CI.

**Deliverables:** Linux and macOS release builds for x86_64 and arm64, multi-arch Linux Docker image (`linux/amd64`, `linux/arm64`), examples and user docs, `publish = false` lifted and an MSRV (`rust-version`) declared, stable config/result version policy, changelog, CI release jobs and troubleshooting guidance.

**Tasks:**
1. Document three distinct load models, request-rate limits, queue expiry, hook lifecycle, state/pooling, threshold semantics and output schemas.
2. Add signal handling, error diagnostics, process cleanup, secret redaction, resource-limit default verification and reproducible config examples.
3. Test Linux/macOS builds and Docker smoke workflow. Validate CI exit statuses, TLS trust and non-root container operation.
4. Run full integration and benchmark regression test matrix; resolve critical bugs; declare MVP contract/version.

**Success criteria:** New user can install, validate a YAML/JSON scenario, run a smoke/load test and consume reliable summary/JSON+exit code in CI on supported platforms.

**Verification:** End-to-end release smoke test in Linux/macOS and Docker; `cargo fmt/clippy/test`; documented benchmark regression checks.

## 3. Cross-phase test and review discipline

Every phase-specific plan created under `plan on phase N` must include:

- Explicit affected files **based on actual repo inspection**, bounded task checklist, tests, local verification commands and rollback considerations.
- At least one expected-success case, invalid-input/failure case and resource-boundary case for affected features.
- A short design decision record for provisional assumptions resolved in that phase, linked back to `ARCHITECTURE_NOTES.md` only if material.
- No silent redefinition of work units: **iterations**, logical request steps, physical HTTP attempts and schedule drops are distinct.
- Security review for untrusted config/targets, redaction, timeouts and memory limits relevant to changes.
- A verified end-to-end CLI behavior once a feature is user-visible.

Repository check commands (the same ones CI runs; see `README.md` and `.github/workflows/ci.yml`):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked   # --all-targets skips doctests
cargo build --workspace --release       # release build (not run in CI yet)
```

The intended user-facing executable is `vush` (for example, `vush run test.yaml` and `vush validate test.yaml`). The Cargo packages are `vush`, `vush-core` and `vush-http` (decided in Phase 0), plus `vush-config` (decided in P1.1, created in P1.2); command flags will be finalized in the relevant phase. Feature-specific integration/benchmark commands are deliberately deferred to the phase spec when fixtures/harness names exist; do not invent existing paths or executable names.

## 4. MVP dependency / scope map

| Requirement | Primary phase(s) |
|---|---|
| Linux/macOS/Docker/CLI foundation | 0, 8 |
| YAML/JSON config and common plan | 1 |
| HTTP/1.1, HTTP/2, HTTPS and request options | 2 |
| Stateful multi-step flow, JSON/header extraction, tokens, assertions, wait, branch, loop | 3 |
| Global/scenario/VU hooks and scope | 3, 4 |
| Persistent VU state, configurable transport isolation, fixed/ramping VUs | 4 |
| Fixed/ramping iteration arrival and request start rate, global/per-scenario budgets | 5 |
| Queue bounds, drop-newest, TTL, drop diagnostic sink and full drain | 5 |
| Metrics, thresholds, reports, JSON, exit codes | 6 |
| Benchmark comparison against k6, high VU investigation | 7 |
| Supported platform release and docs | 8 |

## 5. How to hand off to the coding agent

1. Put these docs at the repository's canonical discovered paths. Do not overwrite existing files without reconciling them.
2. Confirm one root `AGENTS.md`, `CLAUDE.md`, or `GEMINI.md` exists; if not, the user's planning workflow should request approval to create it.
3. Ask: **`plan on phase 0`** (or the earliest incomplete phase). This exact wording activates the user's skill.
4. Agent reads actual source, docs, existing specs/plans; runs bounded brainstorming for this **one** phase; writes a phase-specific spec and executable task plan; updates `BACKLOG.md`; then **stops before implementation** and requests confirmation.
5. After implementation is approved and completed through a separate implementation workflow, verify acceptance criteria, check off backlog items, and proceed to next phase.

## 6. Future backlog / roadmap beyond MVP

### Next release candidates

- [ ] JavaScript/TypeScript **plan-generation** adapter with static/declarative fast path.
- [ ] Python plan-generation adapter with explicit interpreter isolation and startup-cost budgets.
- [ ] Optional bounded dynamic execution hooks (slow path) for behaviors not expressible declaratively.
- [ ] Protocol adapters: WebSocket, gRPC, TCP/UDP; test capability-based protocol interface.
- [ ] Improved reporting sinks (OpenTelemetry/Prometheus) and CI templates.

### Longer-term research

- [ ] Custom natural-human-readable declarative testing language; defer syntax, grammar and AST until product research.
- [ ] Scripting-language user-defined protocol handlers and plugin API with deterministic isolation/budgets.
- [ ] Distributed multi-node test orchestration, coordinated rate budgets and merged metrics.
- [ ] Windows support, browser/GUI, remote dashboards and recorded workload import (HAR/Postman).
- [ ] Optional drop-event archival and replay-for-debug tooling, separate from actual load scheduling; no automatic late replays.
