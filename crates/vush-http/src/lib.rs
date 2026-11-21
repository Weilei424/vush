//! HTTP protocol executor for Vush.
//!
//! [`Executor`] implements `vush_core::protocol::http::HttpExecutor` for
//! HTTP/1.1, HTTP/2 and TLS. Each `send` makes exactly one request on the
//! wire, or none when it fails in `prepare`. It never follows a redirect and
//! never retries, because core's driver owns both loops. This crate owns the
//! redirect rules (`follow`), connection pooling, per-hop timeouts, response
//! bounds, and the per-user session, which holds the user's cookie jar. It
//! must not schedule work or own global counters: scheduling belongs to
//! `vush-core`, and aggregation to the metrics layer.
//!
//! # Building executors
//!
//! [`Executors::new(&plan, test_dir)`](Executors::new) builds a run's
//! executors from a [`WorkloadPlan`] and the test file's directory: one for
//! the file-level hooks, and one per scenario. Under `pool: shared`, scopes
//! with the same version and TLS trust share a pool, and scopes whose
//! version or trust differ never share a connection. [`Executor::new`]
//! builds a lone executor from one [`HttpSettings`], with a pool of its own,
//! and reads only `max_cookies` from the [`ResourceLimits`] it is given.
//!
//! Both trust the system's root certificates plus those in `tls.ca_file`,
//! which is resolved against the test file's directory, and both have a
//! `with_roots` that takes a stand-in for the system's roots. Building fails
//! with a [`BuildError`] when a `tls.ca_file` can't be used, and `new` also
//! when settings that verify have no trusted root. Building needs no
//! runtime, but `send` must run inside a Tokio runtime.
//!
//! A [`Session`] belongs to the executor that made it. It holds the user's
//! cookie jar, unless `cookies: false` or `max_cookies: 0` leaves it none,
//! and under `pool: vu` the user's own connections and TLS session cache. A
//! clone copies the jar. Under every pool, `send` refuses a session from
//! another executor, as `invalid_request` in `prepare`, before sending
//! anything.
//!
//! ```no_run
//! use std::path::Path;
//! use std::time::Duration;
//!
//! use vush_core::plan::{HttpSettings, ResourceLimits};
//! use vush_core::protocol::http::{HttpExecutor, HttpRequest, RequestUrl};
//! use vush_http::Executor;
//!
//! # async fn run() -> Result<(), vush_http::BuildError> {
//! let executor = Executor::new(
//!     &HttpSettings::default(),
//!     &ResourceLimits::default(),
//!     Path::new("."),
//! )?;
//! let mut session = executor.new_session();
//! let request = HttpRequest {
//!     method: "GET".parse().expect("GET is a method"),
//!     url: RequestUrl::Absolute("https://api.example.test/health".to_owned()),
//!     query: vec![],
//!     headers: vec![],
//!     body: None,
//! };
//! let hop = executor
//!     .send(&mut session, &request, Duration::from_secs(10))
//!     .await;
//! if let Ok(response) = &hop.outcome {
//!     println!("{}", response.status);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! [`HttpSettings`]: vush_core::plan::HttpSettings
//! [`ResourceLimits`]: vush_core::plan::ResourceLimits
//! [`WorkloadPlan`]: vush_core::plan::WorkloadPlan

mod body;
mod classify;
mod connector;
mod cookie;
mod executor;
mod executors;
mod follow;
mod json;
mod pool;
mod prepare;
mod stage;
mod text;
mod tls;

pub use executor::{BuildError, CaFileProblem, Executor, Scope, Session};
pub use executors::Executors;
