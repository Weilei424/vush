# Backlog — Vush

> **Status:** MVP planning baseline, 2026-11-01. Phase 0 complete 2026-11-01 (CI green on all four targets); Phase 1 complete 2026-11-11 (CI green on all four targets); Phases 2–8 remain baseline planning. `IMPLEMENTATION_PLAN.md` defines phase goals and acceptance criteria; `ARCHITECTURE_NOTES.md` is the architecture reference. This is a phase/task checklist, **not** a substitute for phase-specific design specs/plans.

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
>
> P1.2 spec: `docs/superpowers/specs/2026-11-03-phase-1-2-yaml-json-reader-design.md` · Plan: `docs/superpowers/plans/2026-11-03-phase-1-2-yaml-json-reader.md` (local only).
>
> P1.3 spec: `docs/superpowers/specs/2026-11-06-phase-1-3-plan-validation-design.md` · Plan: `docs/superpowers/plans/2026-11-06-phase-1-3-plan-validation.md` (local only).
>
> P1.4 spec: `docs/superpowers/specs/2026-11-08-phase-1-4-validate-and-diagnostics-design.md` · Plan: `docs/superpowers/plans/2026-11-08-phase-1-4-validate-and-diagnostics.md` (local only).

- [x] P1.1 Agree versioned YAML/JSON configuration shape and typed immutable workload plan (design approved 2026-11-02, finished 2026-11-03).
  - [x] Value types in `vush_core::plan`: durations, rates, sizes and percentages, parsed from text with exact-value and rejection tables (plan Task 1).
  - [x] `Template` and `VarRef`: `${...}` parsing with `env`, `global`, `scenario`, `vu` scopes and `$${` escaping (plan Task 2).
  - [x] `JsonPath`: the `$.a.b[0].c` subset (plan Task 3).
  - [x] Plan data model: HTTP settings and limits with schema defaults, the three load models, steps, comparisons and thresholds (plan Task 4).
  - [x] `WorkloadPlan::new` as the only constructor: reports every structural error with a file-key path; the plan is read-only and `Send + Sync` (plan Task 5).
  - [x] `docs/config-schema.md`: schema v1 reference with one complete YAML + JSON example, built through the public API in a test (plan Task 6).
  - [x] Replace `ARCHITECTURE_NOTES.md` §5 with a pointer to the schema doc; refresh local `CLAUDE.md`/`AGENTS.md` (plan Tasks 6–7).
  - [x] All four check commands pass and `vush-core` still has no dependencies (plan Task 7).
  - [x] CI green on all four runners plus `fmt` after the user pushes (plan Task 7). Run 41595459312 on `176c882`, 2026-11-03.
- [x] P1.2 Parse equivalent YAML/JSON into the same plan, with `saphyr-parser` as the YAML library and no Serde. Creates `crates/vush-config` (decided 2026-11-02, design approved 2026-11-03, finished 2026-11-05).
  - [x] Crate scaffold, `ConfigError`, the located value tree and the reading budget (plan Task 1).
  - [x] One number rule for both formats, and the rule for unquoted YAML values (plan Task 2). Integers outside the range `i64::MIN` to `u64::MAX` are errors, never stored as `Float`. `010`, `0x10`, `.inf` and similar forms are errors.
  - [x] Strict JSON reader with locations, cross-checked against `serde_json` in tests (plan Task 3).
  - [x] YAML reader: aliases, merge keys, rejected forms and limits (plan Task 4). Duplicate map keys are errors in both formats.
  - [x] Compile `http` and `limits`, merged over the file level (plan Task 5).
  - [x] Compile the three load models (plan Task 6).
  - [x] Compile steps, requests, comparisons and conditions (plan Task 7).
  - [x] Compile thresholds and the whole file, add `parse` and `load`, and locate `PlanError`s (plan Task 8).
  - [x] Whole-crate tests: the schema example from both formats equals a hand-built plan, plus equivalence, diagnostics, limits, secrets and robustness (plan Task 9).
  - [x] `docs/config-schema.md` file rules, local docs and all four check commands (plan Task 10).
  - [x] CI green on all four runners after the user pushes (plan Task 10). Run 42150486584 on `33259a5`.
  - [x] Fixes from Codex's review (2026-11-05): a tagged `<<` key is an ordinary key, not a merge key.
  - [x] CI green on the first review-fix commits. Run 42156507464 on `c032215`.
  - [x] A fixed load key mixed with `stages` is reported at the later of the two keys, with the usual conflict message (user decision after Codex's second review, 2026-11-05).
  - [x] CI green on the load-conflict commits after the user pushes. Run 42162887348 on `7fce79e`.
- [x] P1.3 Validate durations, rates, load modes, loops, bounds, variable scopes and unsupported fields. All rules run in `WorkloadPlan::new` (design approved 2026-11-06, finished 2026-11-07).
  - Decided 2026-11-06:
    - A teardown's `extract` writes the same scope as its setup.
    - References are checked for scope and name, not order.
    - One duration ceiling of `720h` (30 days).
    - `status: []`, `all: []` and `any: []` are errors.
  - "Unsupported fields" needs nothing new, because P1.2 already rejects unknown keys. The nesting limit moved to P1.2.
  - [x] `PlanError.path` becomes a list of segments, errors move to `plan/error.rs`, and checks move to `plan/check/` (plan Task 1).
  - [x] Durations, rates and loads: the `720h` ceiling, zero timeouts and deadlines, zero fixed rates and `max_request_rate`, idle and zero-length ramps, and inherited timeouts reported once (plan Task 2).
  - [x] Requests, conditions and thresholds: `attempts` of 2 or more, the retry method rule, status codes 100–599, empty `status`, `all` and `any`, `${` in a request `name`, a `path` needing a `base_url`, and thresholds naming a real scenario (plan Task 3).
  - [x] Variables: `extract` names, and a scope and name check for every reference, with hints and positions (plan Task 4).
  - [x] Whole-crate tests through `parse`: every rule located, unusual keys escaped, and a `secrets.rs` row per new kind (plan Task 5).
  - [x] `docs/config-schema.md` rules, local docs and all four check commands (plan Task 6).
  - [x] Fixes from the final review (2026-11-07):
    - `WorkloadPlan::new` records at most `MAX_PLAN_ERRORS` (100) errors, then one `TooManyErrors`.
    - Path keys longer than `MAX_PATH_KEY_BYTES` (256) are cut to a `TruncatedKey`.
    - Before these fixes, a 9 KB alias-heavy file reached 831 MB. It now peaks at 69 MB, with the same error output.
  - [x] CI green on the P1.3 commits through the final-review fixes. Run 42408832373 on `2973bb1`.
  - [x] Fixes from Codex's review (2026-11-07):
    - **Nesting bound** (user decision: formalize the bound rather than enforce depth in core). The checks, and dropping a draft, recurse. `WorkloadPlan::new` now documents its tested limit: drafts nested up to 1,000 levels of `if`, `repeat`, `not` or JSON arrays validate on a 2 MiB stack, and a core test pins this. `vush-config` caps files far below that.
    - **Long keys that share a prefix.** A cut path key also keeps the key's length and an FNV-1a fingerprint, so two long keys with the same 256-byte prefix point at their own lines.
  - [x] CI green on the first Codex fixes. Run 42411279685 on `3e20947`.
  - [x] Fixes from Codex's second review (2026-11-07). These are inputs only a direct adapter can build, since the readers already reject them:
    - **Hand-built `var:` fields.** A `var:` field can hold any `VarRef`. Its name is now checked as an `extract` name is (`InvalidVariableName`, `ReservedVariableName`), so `VarRef::Env("")` no longer passes.
    - **Non-finite numbers.** A new `NonFiniteNumber` error rejects NaN and infinity in `json` bodies and in `equals`, `not_equals`, `gt`, `gte`, `lt` and `lte`.
    - **Audit.** A check of every public plan type found no other way past the parsers. `Template`, `JsonPath`, `Ratio` and `HttpMethod` can only come from their parsers.
  - [x] CI green on all four runners after the user pushes the second round of Codex fixes (plan Task 6). Run 42413645903 on `150d504`.
- [x] P1.4 Implement `vush validate` and meaningful config diagnostics (design approved 2026-11-08, finished 2026-11-08).
  - Decided 2026-11-08:
    - `vush validate` only. `dry-run` moves to the phase that adds `vush run`, because it also needs run-time checks.
    - One block per problem, in the style of `rustc`. No source lines, because those would print values from the file.
    - One or more files per command. Exit status 0 if every file is valid, 2 otherwise.
  - Diagnostics stay bounded and keep P1.2's rule: no value from the file appears, only key paths, with keys escaped and cut to 64 characters. `ValueError`'s `Debug` output still prints all of `input`, so don't surface it through `{:?}`, `unwrap` or `expect` on file input.
  - [x] Clearer messages: text forms and word lists name themselves on a wrong type, and a bad `<<` value reached through an alias points at the `<<` (plan Task 1).
  - [x] "Did you mean" hints on unknown keys and step kinds, offering only keys valid at that spot (plan Task 2).
  - [x] `vush validate <FILE>...`: problem blocks, status lines and exit codes, tested through the binary (plan Task 3).
  - [x] README link to `docs/config-schema.md`, the schema doc, architecture notes, local docs and all four check commands (plan Task 4).
  - [x] Fixes from the final review (2026-11-08):
    - A hint never offers a key that would conflict with one already in the map, such as `url` beside `path`, or `stages` beside a fixed load's keys.
    - The list of keys to offer is built once per map, and only when a hint is needed.
    - `docs/config-schema.md` says a bad `<<` value reached through an alias is reported at the `<<`.
  - [x] CI green on all four runners after the user pushes (plan Task 4). Run 42503992380 on `8faab92`.
  - [x] Fix from Codex's review (2026-11-08): usage errors and help escape control characters in every argument and in the program name, so a newline can't forge an output line. Arguments are escaped rather than rejected, because a real file name can hold a control character.
  - [x] CI green on the Codex fix after the user pushes. Run 42505419884 on `3969d19`.
- [x] P1.5 Add fixtures, examples and checks of the schema doc against the code (design approved 2026-11-09, finished 2026-11-11). P1.5 closes Phase 1.
  - Decided 2026-11-09:
    - Valid examples go in `examples/`, and test-only broken files go in `crates/vush/tests/invalid/`. The schema doc's complete example is checked straight from the doc, not copied.
    - Each broken file's exact `vush validate` output is recorded in a golden `.stderr` file and compared byte for byte. `VUSH_BLESS=1 cargo test -p vush --test fixtures` rewrites the goldens, and it never hides a failure.
    - The schema doc owns the schema. Code that disagrees with it gets fixed. A documented default or limit changes only with the user's approval.
  - [x] Pin the UTF-8 error locator's CR, CRLF, byte-order-mark and multibyte branches, and check one invalid file through the binary (plan Task 1).
  - [x] `examples/` holds smoke, flow and rate tests in YAML and JSON. Each pair builds the same plan. All six pass `vush validate`, and so does the schema doc's complete example (plan Task 2).
  - [x] 21 broken files with byte-for-byte golden output, folder guards and a mixed run (plan Task 3).
  - [x] Every default in the schema doc's tables and prose is checked against the plan the compiler builds. The File rules limits and `720h` are checked against the constants (plan Task 4).
  - [x] The README points to `examples/` and the bless command, and the roadmap's source tree lists `examples/` (plan Task 5).
  - [x] Fixes from the reviews (2026-11-10 and 2026-11-11):
    - A bless run lists the goldens it rewrote. It writes straight to stderr, because `cargo test` hides `eprintln!` output from passing tests.
    - The `fixtures.rs` header and the `several-problems.yaml` comment describe what they hold, and the README says bless re-records the `.stderr` files.
  - [x] CI green on all four runners after the user pushes. Run 43060171180 on `5b34dd4`. Phase 1 is marked complete here, in the README, in `IMPLEMENTATION_PLAN.md` and in `ARCHITECTURE_NOTES.md` (plan Task 5).

## Phase 2 Execution Checklist — HTTP executor

> P2.1 spec: `docs/superpowers/specs/2026-11-11-phase-2-1-protocol-boundary-design.md` · Plan: `docs/superpowers/plans/2026-11-11-phase-2-1-protocol-boundary.md` (local only; `docs/superpowers/` is gitignored).

- [ ] P2.1 Implement protocol-neutral request/observation/error boundary (design approved 2026-11-11).
  - Decided 2026-11-11:
    - One `HttpExecutor::send` call sends one request on the wire, called a hop. Core's driver owns the attempt loop (retries, P2.3) and the hop loop (redirects, P2.2). `vush-http` supplies the redirect rules as a pure `follow`.
    - The trait lives in `vush_core::protocol::http`, and `vush-http` implements it from P2.2. `vush-core` keeps no dependencies.
    - Per-user state lives in a session type that `vush-http` defines and core owns. That state is cookies, plus the user's own pool under `pool: vu`. The executor holds none.
    - There are ten pinned failure labels and four stage labels. A failure carries fixed text, never library text, URLs or values. `Debug` never shows a value.
    - A rejected redirect ends the attempt with `invalid_redirect`, and a chain longer than `http.redirects` (when that is 1 or more) ends it with `too_many_redirects`. Neither one is a hop. With `redirects: 0` the `3xx` is still the response.
    - A throwaway spike fitted the draft trait to reqwest, and to hyper with hyper-util and its own connector. It recommends hyper for P2.2.
  - [x] Failure kinds and stages with pinned labels (plan Task 1).
  - [x] Hop timing, sizes and outcomes, and hop, attempt and step records (plan Task 2).
  - [ ] `JsonValue` and `HttpRequest` with redacted `Debug`, and an owner for every plan field (plan Task 3).
  - [ ] `HttpResponse` and `HttpHop` with redacted `Debug` and `record()` (plan Task 4).
  - [ ] The `HttpExecutor` trait and its contract, a fake executor driven through generics, and `Send` futures (plan Task 5).
  - [ ] Architecture notes, local docs and all four check commands, with `vush-core` still free of dependencies (plan Task 6).
  - [ ] CI green on all four runners after the user pushes (plan Task 6).
- [ ] P2.2 Support HTTP1.1/2 over HTTP/HTTPS; redirects, methods, headers, query and bodies.
  - Check header names and URL schemes before any traffic, and decide how `path` joins `base_url`. P1.3 only requires a `base_url` to exist.
  - From P2.1:
    - Implement `send` and `follow` in `vush-http`. This adds the `vush-http → vush-core` dependency.
    - The spike recommends hyper and hyper-util with Vush's own connector. That connector must reject unsupported schemes itself.
    - Core's hop loop counts redirects against `http.redirects` and gives each hop the attempt's remaining time. It ends a longer chain with `too_many_redirects`.
    - Define how the query, form and JSON bodies are encoded, and the default `Content-Type`.
    - Document the redirect limit and the attempt timeout in `docs/config-schema.md`.
- [ ] P2.3 Support pooling configurations, TLS, explicit timeouts and opt-in retries. Define `pool: shared` when scenarios differ in TLS or HTTP version; a connection opened with verification off must never serve a scenario that kept it on.
  - From P2.1:
    - Build executors per settings profile, and keep the `pool: vu` pool in the session.
    - Core's attempt loop decides which failure kinds and statuses are retried, and where the backoff sleeps. Core needs a timer for that.
    - Decide whether `retry.attempts` and `redirects` need upper bounds, because records grow with them.
    - Choose the rustls crypto provider.
- [ ] P2.4 Guarantee credentials/cookies cannot leak through shared pooled transport.
  - From P2.1:
    - The session's cookie jar, with `max_cookies` and `cookies: false`.
    - What happens when a request carries its own `Cookie` header.
    - `follow` dropping `Authorization` and that header when the origin changes.
- [ ] P2.5 Implement bounded body capture and safe error diagnostics.
  - From P2.1:
    - Whether to drain or close the connection after a cut body. The spike found that dropping the rest of the body closes an HTTP/1.1 connection.
    - Header limits, and the fixed failure texts.
- [ ] P2.6 Add local test-server integration tests for success, failure, delay, redirect, oversized body, TLS and reuse.
  - From P2.1: test the `HttpExecutor` contract against the real executor. Every failure kind needs a real cause, and no fixture case may be reported as `other`.

## Phase 3 Execution Checklist — Scenario interpreter

- [ ] P3.1 Implement ordered step execution and fail-fast per-iteration policy.
- [ ] P3.2 Add per-VU state, JSON/header extraction, interpolation and cookie/token propagation.
- [ ] P3.3 Add status/content/JSON assertions and distinct failure categories.
- [ ] P3.4 Add think time, structured `if/else` and bounded loops.
- [ ] P3.5 Implement global/per-scenario declarative setup/teardown and define their failure policies.
- [ ] P3.6 Enforce body, variable, loop and hook bounds; test auth-flow and failing-flow cases.
  - Decide how `max_variables` relates to the names a scenario extracts. P1.3 doesn't compare them.

## Phase 4 Execution Checklist — Stateful VUs and closed scheduling

- [ ] P4.1 Implement VU lifecycle/ownership; one iteration at a time; persistent state by default.
- [ ] P4.2 Implement fixed and ramping VU schedules; avoid one OS thread per VU.
  - Decide a ceiling on VU counts from measurements. P1.3 sets none.
- [ ] P4.3 Add configurable per-iteration state reset and shared/scenario/VU connection scope.
- [ ] P4.4 Implement per-VU setup/teardown and cleanup on retirement/scale-down.
- [ ] P4.5 Verify isolation under concurrency, memory bounds and closed-loop semantics.

## Phase 5 Execution Checklist — Open/load control and overload

- [ ] P5.1 Approve explicit request-start rate semantics for workflows, global/per-scenario budgets and retry accounting.
- [ ] P5.2 Implement fixed/ramping scenario-iteration arrival, independent of completion.
- [ ] P5.3 Implement request-start pacing; report target vs achieved rate and scheduler lateness.
  - Decide a rate ceiling from measurements. P1.3 sets none, so until then rate arithmetic must not overflow for any `u64` rate. Durations are capped at `720h`.
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
- [x] Phase 1: exact source-location validation behavior. Schema names and provisional default limits decided 2026-11-02 (P1.1). Decided 2026-11-03 (P1.2): every error carries a key path and a line and column in both formats, and never quotes a value from the file.
- [x] Phase 2: where the protocol-neutral executor trait lives. Decided 2026-11-11 (P2.1): `HttpExecutor` in `vush_core::protocol::http`, implemented by `vush-http`, so crate dependencies stay acyclic and `vush-core` has none. The plan compiler's home is decided: `crates/vush-config` (2026-11-02, P1.1), so `crates/vush` doesn't need a library target for it.
- [ ] Phase 2: maintained HTTP/TLS dependencies and retry/pooling implementation. The YAML library is decided: `saphyr-parser` (2026-11-03, P1.2). Retries run in core's attempt loop (decided in P2.1). The P2.1 spike recommends hyper + hyper-util with Vush's own connector over reqwest, and P2.2 decides.
- [ ] Phases 3–4: hook failure/cleanup precedence, state reset semantics, VU worker ownership. Also whether `all` and `any` stop at the first decisive condition (the schema example relies on it), whether `vu.id` and `vu.iteration` start at 0 or 1, what exceeding `max_response_size` or `deadline` does, and how `exists` treats JSON `null`.
- [ ] Whichever phase adds `vush run`: a `--dry-run` that makes every check a run makes before traffic, such as `${env.NAME}` being set and `tls.ca_file` being readable. It never prints the plan's `Debug` output, which holds literal header and body text. Moved from Phase 1 on 2026-11-08.
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
