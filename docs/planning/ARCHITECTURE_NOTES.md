# Architecture Notes — Vush

> **Status:** Proposed MVP architecture (v0.1, 2026-11-01), updated after Phase 0. This is a baseline for phase-by-phase agent planning, **not** an as-built description of the engine: only the Phase 0 foundation (workspace, crate layout, toolchain pin, CI) exists so far. Names, configuration examples, and implementation libraries are illustrative unless explicitly marked as requirements or decided.
>
> **Project name:** **Vush** (confirmed 2026-11-01). **CLI binary:** `vush` (e.g., `vush run test.yaml`). Repository: `https://github.com/Weilei424/vush`. Cargo packages: `vush`, `vush-core`, `vush-http` (decided in Phase 0), plus `vush-config` (decided in P1.1, created in P1.2). Final command flags remain to be determined during the relevant phase. This document is canonical for architecture decisions; `IMPLEMENTATION_PLAN.md` owns phase scope and `BACKLOG.md` owns execution status.

## 1. Vision and scope

Build a performance-focused load- and smoke-testing CLI written in Rust, optimized for high request-generation throughput, low CPU/memory overhead per virtual user (VU), high concurrency, accurate workload timing, and quick startup. Users define tests initially in **YAML or JSON**, both mapping to the same internal workload model.

### Required for MVP

- Runs on Linux (x86_64, arm64) and macOS (Apple Silicon, Intel), plus a Linux Docker image for `linux/amd64` and `linux/arm64`. CI builds, lints and tests natively on all four targets from Phase 0 onward.
- HTTP/1.1, HTTP/2, HTTPS/TLS, connection reuse, configurable connection-pool isolation.
- HTTP methods, URLs, headers, bodies, credentials/cookies, redirects, timeouts, and explicit opt-in retries.
- Stateful, multi-step scenarios with variable interpolation; JSON and header extraction; cookies/tokens; response assertions; pauses; structured conditional branches; bounded loops.
- Global, per-scenario, and per-VU setup/teardown lifecycle hooks (declarative workflows for MVP, not arbitrary embedded scripting).
- Three **distinct** load drivers: fixed/ramping concurrent VUs (closed); fixed/ramping **scenario-iteration starts** (open); target or cap on **HTTP request starts**, including multi-step workflows. Global and per-scenario request-rate controls.
- Bounded admission queues and bounded in-flight work. New arrivals are dropped if queue is full; accepted work can expire after a configured maximum wait. Sampled asynchronous drop-event diagnostics with authoritative loss counters.
- Default persistent state per VU; state reset/persistence configurable. Default shared HTTP transport pool; per-scenario/per-VU isolation configurable independently of state.
- Low-overhead aggregate metrics by default, optional detailed sampled/per-request records, thresholds, text summaries, stable JSON output, useful CLI exit statuses.
- Configurable graceful stop: stop scheduling at end of duration, **drain accepted non-expired work**, and await in-flight requests. Every request/hook must have a finite timeout; process interruption is a separate path.
- Comparative benchmarks against Grafana k6 under identical workloads plus absolute measurements of throughput, resource use, and scheduler accuracy.

### Explicitly not in MVP

JavaScript/TypeScript and Python runtimes, user-defined scripting handlers, a custom human-language-like DSL or its grammar/AST, non-HTTP protocols, runtime protocol plugins, distributed orchestration, a GUI, Windows binaries, persistent job storage, and DLQ replay. Retain stable component boundaries so these can be introduced later without replacing the engine.

### Performance aspirations, not promises

- Efficient very-high-concurrency operation, with **100,000 mostly idle/sleeping VUs** as an eventual *benchmark target*, not an unconditional supported limit.
- Maximize throughput and minimize per-VU memory/CPU while preserving trustworthy timings.
- Define numerical performance budgets from repeatable benchmarks in Phase 7; do **not** invent throughput, memory, or latency guarantees in advance.

## 2. Architectural principles

1. **Rust owns the hot path.** Input adapters compile into an immutable, validated execution plan; execution does not interpret YAML/JSON at request time. Future languages should primarily construct plans, with explicit opt-in dynamic hooks on a slower path.
2. **Distinct layers.** Configuration parsing, planning/validation, scheduling, scenario execution, protocol transport, VU state, and telemetry have separate interfaces.
3. **Bound everything.** Queues, active tasks/iterations, loops, HTTP response capture, retained variables, runtime memory, event logging, and per-request/hook duration must be bounded or externally configurable with safe defaults.
4. **Measure generator shortcomings.** Arrival deadlines, actual admission/start times, scheduling lateness, rejection/expiry, achieved vs target throughput, and test wall time are separate measurements.
5. **No silent workload substitution.** Drops are not retried from an event sink; HTTP retries are opt-in and counted as additional physical attempts; never silently substitute request-rate for iteration-rate semantics.
6. **Isolate user context from transport.** Per-VU cookies, auth, and variables must not leak through shared clients/pools.
7. **No unbounded instrumentation.** Aggregation is the default; record-level output is gated and sampling/filtering is explicit.
8. **Phase plans can decide details.** Architecture invariants are binding; crate names, chosen libraries, exact config key names, and algorithms are provisional until the relevant phase design is approved.

## 3. Logical architecture

```text
YAML/JSON -> Parser -> Schema Validator -> Plan Compiler -> Immutable WorkloadPlan
                                                        |
Global Setup -> Scenario Setup -> VU Setup              v
                                         Workload Coordinator / Scheduler
                                          |      |           |
                               VU closed  |  Iteration   | Request-start pacing
                                          v      v           v
                                  Bounded admission / permits
                                          |
                                  ScenarioRunner (per iteration)
                                  |   |       |        |
                            VU state  flow logic  assertions  extraction
                                          |
                              ProtocolExecutor trait/interface
                                          |
                                 HTTP adapter/client pools
                                          |
                            Metrics events / local aggregation
                              |                         |
                         Reports                    DropEventSink
                              |
                            Teardown + exit status
```

*Diagram is logical, not a prescription for a centralized mutex/queue. Closed-model VUs may bypass an open-model arrival queue; use bounded permits or sharded queues when appropriate.*

### Component contracts

| Component | Owns | Must not own |
|---|---|---|
| CLI | Commands, invoking `vush-config`, output selection, process exit mapping, signals | HTTP flow execution, parsing test files |
| Input adapter / Plan Compiler (`vush-config`) | YAML/JSON decoding, defaults, strict validation, source locations in errors, compilation into typed, immutable plan | Runtime network I/O or VU scheduling |
| `WorkloadPlan` (`vush_core::plan`) | Typed, read-only plan with all text forms parsed and defaults resolved; `WorkloadPlan::new` is the only way to get one | Parsing libraries, serde, reading the environment, clock or files, holding secret values |
| Workload Coordinator | Global lifecycle, scenario activation, stop/drain sequencing, fairness and capacity budgets | HTTP protocol details |
| Scheduler / Rate Controller | Closed/open load models, intended timestamps, ramp stages, per-scenario/global request-start budgets, admission/expiry accounting | Parsing and response validation |
| VU Manager | VU identity, persistent/resettable state, reuse, resource limits, per-VU lifecycle | Global queue policy |
| ScenarioRunner | Ordered steps, bounded if/loop semantics, interpolation, fail-fast iteration behavior, think time, hooks | Owning HTTP socket pools or aggregate reporting |
| ProtocolExecutor interface | Protocol-neutral execute request/operation and return typed observations | Scheduling VUs |
| HTTP Adapter | HTTP 1.1/2, TLS, redirects, pooling, timeout/retry, streaming/response bounds | Scheduling or global counters |
| Metrics Collector | Low-contention counters/histograms/gauges; merge, snapshots, labeling | Full response retention by default |
| DropEventSink | Best-effort bounded sampled records of rejected/expired work | Truth source for drop counts; replay |
| Reporter | Human summary, versioned JSON, thresholds and CI exit-code mapping | Modifying ongoing load |

### Rust workspace structure (crate layout decided in Phase 0)

A Cargo workspace with clear ownership boundaries and a modest crate count rather than premature micro-crates:

```text
Cargo.toml          # virtual manifest: shared package fields, lints, dependency versions
Cargo.lock
rust-toolchain.toml # pinned toolchain shared by local development and CI
.github/workflows/ci.yml  # fmt + clippy/test matrix on the four supported targets
crates/
  vush/            # binary: CLI, reports and command handling
    tests/         # CLI integration tests
  vush-config/     # YAML/JSON reading, defaults, file:line errors, builds the plan (from P1.2)
  vush-core/       # typed workload plan, scenario interpreter, VU state, coordinator
  vush-http/       # HTTP ProtocolExecutor implementation
  vush-metrics/    # optional later extraction from core when justified by dependencies
examples/          # later phases
benchmarks/        # or benches/ per actual harness (Phase 7)
docs/planning/
  ARCHITECTURE_NOTES.md
  IMPLEMENTATION_PLAN.md
  BACKLOG.md
```

The workspace manifest, the three crates, the toolchain file and the CI workflow exist as of Phase 0. `vush-config` arrives in P1.2. `vush-metrics`, `examples/` and the benchmark directory are still proposals; confirm against the actual tree before planning a phase. Root agent-instruction files (`CLAUDE.md`, `AGENTS.md`) are local-only and gitignored.

Potential dependencies to assess per phase: Tokio for async runtime/tasks/time; a mature async HTTP stack (e.g., reqwest atop hyper) with TLS/HTTP2; Serde for typed JSON/config; an actively maintained YAML parser; clap for CLI; bounded histograms or equivalent for quantiles. Evaluate maintenance, version, licensing, and memory/throughput before committing. Avoid version pins in this baseline.

## 4. Runtime semantics / contracts

### 4.1 Work unit and time definitions

- **Scenario:** an immutable ordered/structured flow of steps and scenario-level load settings.
- **Iteration:** one run of a scenario's flow by a VU; may contain 0..N physical HTTP attempts according to branches/retries.
- **Request start:** instant a physical HTTP attempt begins after rate admission; distinguish from a logical request step and from request completion.
- **Scheduled timestamp:** intended iteration/request time from the workload plan (monotonic clock).
- **Active load window:** interval in which new work is scheduled (e.g., 60s).
- **Drain window:** afterward, queue drains and in-flight operations finish/timeout; no new scheduled arrivals.
- **Total run time:** setup + active window + drain + teardown; reported separately from the configured active duration.

Record both `scheduled_at` and `started_at` where the scheduling model applies. Treat per-request and per-iteration timings independently, using a monotonic timer.

### 4.2 Load models

1. **Closed VU model:** N persistent VUs independently loop through scenarios; each VU starts the next iteration only after its previous iteration ends. Supports fixed VUs and ramping active VU counts. A fixed run stops after a duration or after a per-VU iteration count (the smoke-test form).
2. **Open iteration-arrival model:** planned new scenario iteration starts at fixed/ramped rate, independently of iteration completion. An admitted iteration acquires an available bounded VU worker; each VU retains its own state on reuse. No overlapping iterations on one VU. Reject when admission capacity exhausted.
3. **Request-start rate model:** explicit workload control for physical HTTP request starts, not complete iteration starts. It is implemented at the HTTP step boundary with configurable global/per-scenario budgets and a defined VU/iteration demand source. Complex flows may be unable to achieve a requested target because dependent steps are serial; report achieved vs target and waiting/backlog. Configure this as an explicit distinct mode or pacing policy; **never** imply iteration rate equals request rate.

Initial default for request rate: rate is a **non-guaranteed target with bounded burst/admission**, rather than a promise or infinite catch-up. Phase 5 design must nail down strict-cap vs target-rate options, fairness across scenarios, pacing algorithm, and how requests are counted when HTTP retries occur. Do not pretend these are resolved.

### 4.3 VU state and transport

- VU state is isolated per user: variable namespaces, captured data, cookies, credentials/tokens, loop-local values; defaults persist across iterations. Config can reset per iteration or by other explicit scope.
- A `WorkloadPlan` is immutable/shared; mutable VU state is exclusively owned by one active VU task at a time.
- Shared HTTP client/pool is the default, with configurable per-scenario/per-VU pooling. Sharing transport does **not** share cookies/credentials; avoid a global shared cookie jar. HTTP/2 multiplexing may share sockets where permitted.
- Treat variable interpolation as typed or explicitly encoded, never automatically print secrets; label scopes for global setup data (read-only snapshot), scenario setup, and per-VU mutable data.
- Limit size/count of variables, cookies, response buffers, and allowed extraction results. Defaults/limits must be documented and tested.

### 4.4 Scenario runner

Supported declarative operations in MVP: `request`, `wait`, `if/else`, `repeat` with a bounded count, extract from JSON path/header, assertions on HTTP status/content/selected JSON fields, and interpolation into subsequent requests. With no expected status configured, any 4xx or 5xx response fails the step, so a smoke test of a broken endpoint can't pass silently. A failed required step aborts the **current iteration**, records the reason, then permits a subsequent iteration. There is no automatic continuation through a failed token dependency: a missing extracted value or unset variable fails the step and is never replaced with an empty string.

Setup/teardown scopes: global once, per-scenario once per run, and per-VU upon allocation/disposal. All use the same bounded declarative operations with constraints appropriate to the scope. **Provisional failure policy:** global setup failure aborts all scheduling; scenario setup failure disables that scenario while other scenarios may run; per-VU setup failure retires that VU and records failure; teardown failure affects run status and report, not retrospective request metrics. Finalize in Phase 3/4.

Loops must have static/configured maximum iterations and execution deadline; condition expressions are limited structured comparisons/boolean composition, **not** an embedded general-purpose interpreter. Custom human-readable DSL is explicitly postponed.

### 4.5 Queue overload, expiry, drain and dropped-work events

- Open arrivals have bounded admission queue and bounded active-iteration capacity. On full queue: immediately reject **new** arrival; increment an authoritative counter with reason `queue_full`.
- Accepted queue entries may expire before execution if their configurable max delay is exceeded; increment `expired_before_start`. They **must not** be silently executed late.
- On active-window close: stop producing new arrivals, drain **accepted non-expired** entries, wait for all in-flight operations to complete within their individual finite timeouts, and then run teardown. Expired entries are accounted as not executed.
- No automatic replay or retry of dropped/expired arrivals. Do not enqueue an entire payload/body in telemetry. Sampled drop records (scheduled timestamp, scenario, reason, wait, metadata) flow through a separate bounded nonblocking sink. If diagnostics sink overflows, increment `drop_events_lost`; authoritative `dropped_work` counters are independent.
- Emergency signal handling (SIGINT/SIGTERM), hard cancellation, and shutdown deadlines require explicit policies in Phase 5/8; normal completion must not kill in-flight work.
- Never let the diagnostic sink or a single queue lock constrain the traffic hot path unnecessarily.

### 4.6 HTTP and retry semantics

- Support HTTP/1.1, HTTP/2, TLS/HTTPS; redirect policies; timeouts; connection reuse; request bodies/headers/auth/cookies.
- One logical HTTP step may produce multiple physical attempts only if retry policy is explicitly enabled. Report logical-step success/failure **and** physical attempt counts and timings separately. Retry can alter achieved traffic; rate policies must specify whether they apply to physical attempts (recommended) and account accordingly.
- Stream or limit response bodies. Extract/validate necessary data before dropping buffers; only retain full responses on explicit opt-in and within limits.
- Avoid leaking auth headers, session cookies, passwords, tokens, or sensitive bodies into metrics, failure diagnostics, and JSON output.

### 4.7 Metrics and reporting

**Always-on low-overhead metrics:** scheduled/admitted/started/completed/failed iterations; attempted/completed HTTP requests; response status categories; transport errors; assertion failures; dropped work by reason; drop diagnostic events lost; in-flight VUs/requests; scheduler lateness; queue delay/depth; per-request duration; full iteration duration; active-window achieved RPS/IPS; aggregate data sent/received; resource use in benchmark harness.

- Histograms/quantile estimation for p50/p90/p95/p99 without storing all events by default; use bounded, mergeable storage or well-defined approximation.
- Label metrics by bounded dimensions (scenario, static step name, status/error category); avoid unbounded dynamic URL/path/user IDs.
- Optional raw request event recording must be bounded, filtered/sampled and asynchronous, with explicit data-redaction rules.
- Separate latency of network request from scheduler delay, queue delay, iteration time (including think time), and drain time.
- Threshold rules on p95 latency, error rate, request/iteration rate, and dropped-work ratio; any configured failed threshold affects exit status. Distinguish workload generation failures from SUT failures.
- Provide stable, **versioned** JSON results with units and definitions, CLI summary, and machine-readable exit codes. Proposed initial codes: `0` passed, `1` thresholds/assertions failed, `2` config error, `3` generator/runtime failure; exact mapping finalized in Phase 6.

### 4.8 Nonfunctional and safety constraints

- Default finite request/hook timeouts. A slow or malicious target must not indefinitely hang ordinary completion.
- Protect against hostile response sizes, decompression amplification, excessive redirects, unbounded test variables/loop counts, and infinite queue growth.
- Avoid secrets in logs, crash dumps, sampled events, example configs, or versioned output; document environment variable/secret injection.
- User controls which targets are tested. Do not implement stealth/evasion or amplification-oriented features.
- Non-idempotent request retries (e.g., POST) are **off by default** and require explicit user configuration.
- Use deterministic clocks/injected timer or simulated scheduler tests where possible to verify scheduling semantics without flaky sleeps.

## 5. Configuration schema

Schema v1, the YAML/JSON test file format, is defined in [`docs/config-schema.md`](../config-schema.md). If that document and this one ever disagree, it wins. YAML and JSON encode the same typed plan, `vush_core::plan::WorkloadPlan`.

## 6. Quality gates / architectural verification

- **Correctness:** deterministic scenario step ordering, branch/loop bounds, state isolation, extraction and cookie behavior, no unintended request replay, correct HTTP1/2/TLS/timeout semantics.
- **Conservation of scheduled work:** every scheduled arrival is accounted as queued/admitted then started/completed, explicitly rejected, expired, or cancelled; never double count. Distinguish *logical* work and *physical* attempts.
- **Timing:** verify clock and arrival spacing against predictable local fixture; compare intended to achieved load and scheduler lag under forced overload.
- **Graceful completion:** stop new arrivals at deadline, drain admitted non-expired queue, wait for in-flight requests to finish/timeout, run appropriate teardown, and emit accurate active/drain/run durations.
- **Memory:** bounded queues, buffers and telemetry; test sleeping VU memory at several scales and a 100K aspirational benchmark where hardware permits.
- **Comparison:** reproduce equivalent k6 HTTP workloads with identical host, target, workload, connection reuse, and thresholds; report test environment and caveats. Do not claim superiority without evidence.

## 7. Provisional decisions to finalize during phase planning

| Decision still open | Default working assumption | Resolve by |
|---|---|---|
| Exact crate count and filenames | **Decided:** `vush` (bin), `vush-core`, `vush-http` under `crates/`, plus `vush-config` from P1.2 | Phase 0, P1.1 ✓ |
| YAML parser, HTTP library, histogram implementation | Evaluate maintained libraries by feature/performance/security | Phases 1, 2, 6 |
| YAML/JSON schema and error location reporting | Schema v1 **decided** (scenario-owned load, tagged steps); error locations still open | P1.1 ✓, P1.2–P1.4 |
| HTTP connection pool isolation algorithm | Shared by default, scenario/VU variants selectable | Phase 2/4 |
| Setup failure policies and hook data lifetime | Abort global / skip failed scenario / retire failed VU | Phase 3/4 |
| Virtual-user reuse for open arrivals | Persistent bounded reusable VU pool; one iteration per VU | Phase 4/5 |
| Strict rate cap vs target-rate/burst semantics | Explicit mode/policy; no unbounded catch-up | Phase 5 |
| Fairness, budgets, queue topology and expiry | Bounded per-scenario admissions with clear global limits | Phase 5 |
| Threshold/error exit-code precedence | Stable tested mapping documented with JSON schema | Phase 6 |
| Signals vs endless drain | Respect normal drain; define emergency interruption separately | Phase 5/8 |
| Default limits (timeouts, buffers, loops, events) | Provisional values set in P1.1; adjust from measurements | Phases 1–6 |
| Numerical performance SLOs | Benchmark and publish baseline first | Phase 7 |

## 8. Agent collaboration / change control

- Invoke the supplied **Plan On Step** skill only with the exact phrase **`plan on phase N`** (or `plan on step N`). This baseline doc creation does not itself invoke that workflow.
- Before planning a phase, inspect the actual repository and the root `AGENTS.md`/`CLAUDE.md`/`GEMINI.md`; read all three docs here and existing specs/tests. Record resolved doc paths; do not duplicate planning files.
- Propose alternatives for unresolved target-phase decisions, get design approval, write a phase-specific spec and plan, update the checklist, and stop before coding as the skill requires.
- Do not silently change architecture invariants to make implementation easier. If a material deviation becomes necessary, document tradeoffs and obtain approval before implementation.
- Update this file only when a **material** architectural responsibility or default changes. Implementation-local decisions belong to phase specs/plans.

## 9. Backlog / future TODO (non-MVP)

- [ ] JavaScript/TypeScript input adapter compiling declarative workloads; benchmark its parsing/startup overhead.
- [ ] Python input adapter compiling workloads; isolate runtime, dependency and security model.
- [ ] Explicit dynamic scripting hooks for behavior not representable by declarative plans; measure slow-path overhead and limit hot-path usage.
- [ ] Design a new **natural-language-like declarative DSL** only after user research; grammar and AST are intentionally undefined.
- [ ] WebSocket and gRPC executors; then TCP/UDP and other protocols.
- [ ] Extensible protocol custom handlers, including scripting-driven handlers with isolation and runtime budgets.
- [ ] Distributed coordination, shard assignment, synchronized starts, cross-node result merging.
- [ ] Windows packaging, richer CI integrations, Prometheus/OpenTelemetry output and optional visual dashboard.
- [ ] Evaluate optional persisted drop-event archives and offline analysis; **not** automatic replay of rejected load.
- [ ] More sophisticated scenario branching/data sources, HAR/Postman import, and reusable test templates.
