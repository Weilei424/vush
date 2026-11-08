# Vush

**A high-performance smoke and load testing engine written in Rust.**

Vush aims to provide a fast, resource-efficient, and extensible alternative for API smoke testing and load testing, with an emphasis on concurrency, flexible workload scheduling, and developer experience.

## Planned Features

- **High-performance execution** — Rust and asynchronous I/O powered by Tokio.
- **Flexible test definitions** — YAML and JSON, with scripting support planned.
- **Stateful workflows** — Multi-step scenarios, variables, authentication, and assertions.
- **Load scheduling** — Concurrent virtual users, request/iteration rates, and ramp-up/down.
- **Metrics and reporting** — Throughput, latency percentiles, errors, and performance thresholds.
- **Extensibility** — Future support for additional protocols, custom DSLs, and distributed execution.

## Project Status

🚧 **Early development — repository foundation in place; the load-testing engine is not implemented yet.**

See `docs/planning/` for the architecture notes, implementation roadmap, and backlog.

## Validate a Test File

Tests are declared in YAML or JSON files. The format is described in [`docs/config-schema.md`](docs/config-schema.md).

```sh
cargo run -p vush -- validate test.yaml
```

`validate` checks one or more files and sends no traffic. It exits `0` if every file is valid and `2` otherwise. It never quotes a value from the file.

## Development

Prerequisite: [rustup](https://rustup.rs). The pinned toolchain (Rust 1.99.1 with rustfmt and clippy) installs automatically from `rust-toolchain.toml` the first time you run `cargo` in the repository.

Checks (the same commands CI runs):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
```

Run the CLI:

```sh
cargo run -p vush -- --help
```

Run a single test:

```sh
cargo test -p vush --test cli help_flag_prints_usage_and_exits_zero
```

## Supported Targets

| Platform | Target triple | CI runner |
|---|---|---|
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `ubuntu-24.04` |
| Linux arm64 | `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `macos-15` |
| macOS Intel | `x86_64-apple-darwin` | `macos-15-intel` |

CI runs formatting, clippy, and the full test suite natively on all four targets. Windows is not supported.

### Optional: run the tests on arm64 locally

On an x86_64 machine with Docker and QEMU emulation (for example, Docker Desktop), you can run the test suite on arm64 Linux before pushing. It is slow because it is emulated, and CI covers arm64 natively anyway.

```sh
docker run --rm --platform linux/arm64 -v "$PWD":/work -w /work \
  -v vush-arm64-target:/work/target rust:1.99.1 \
  cargo test --workspace --locked
```

The named volume keeps arm64 build output out of your local `target/`. Keep the image tag in sync with `rust-toolchain.toml`.

## Non-goals for the MVP

- JavaScript/TypeScript or Python runtimes and scripting handlers
- A custom test DSL
- Non-HTTP protocols and protocol plugins
- Distributed (multi-node) execution
- A GUI
- Windows support
- Replaying dropped or expired load

Several of these are on the post-MVP roadmap in `docs/planning/`.

## Compatibility Policy

Vush is pre-1.0. Until the 1.0 release, CLI flags, the test configuration schema, and the JSON result format may change without notice. The supported platforms are the four targets listed above.

## License

[Apache-2.0 license](https://github.com/Weilei424/vush/tree/main?tab=Apache-2.0-1-ov-file)
