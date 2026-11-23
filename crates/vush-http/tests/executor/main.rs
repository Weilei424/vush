//! `vush_http::Executor` against servers inside the test process, on
//! 127.0.0.1 only.
//!
//! One test crate, so every fixture in `common` is used by some test.

mod common;
mod compression;
mod contract;
mod cookies;
mod diagnostics;
mod failures;
mod pools;
mod redirects;
mod retries;
mod send;
mod tls;
