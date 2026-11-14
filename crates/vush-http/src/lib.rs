//! HTTP protocol executor for Vush.
//!
//! [`Executor`] implements `vush_core::protocol::http::HttpExecutor` for
//! HTTP/1.1, HTTP/2 and TLS. Each `send` makes exactly one request on the
//! wire, or none when it fails in `prepare`. It never follows a redirect and
//! never retries, because core's driver owns both loops. This crate owns the
//! redirect rules (`follow`), connection pooling, per-hop timeouts, response
//! bounds, and the per-user session that holds cookies. It must not schedule
//! work or own global counters: scheduling belongs to `vush-core`, and
//! aggregation to the metrics layer.
//!
//! # Building an executor
//!
//! [`Executor::new`] builds one from a plan's [`HttpSettings`] and trusts the
//! system's root certificates. [`Executor::with_roots`] trusts exactly the
//! roots it is given. Either fails with a [`BuildError`] for settings that
//! aren't supported yet. An executor needs no runtime to be built, but `send`
//! must run inside a Tokio runtime.
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use vush_core::plan::HttpSettings;
//! use vush_core::protocol::http::{HttpExecutor, HttpRequest, RequestUrl};
//! use vush_http::Executor;
//!
//! # async fn run() -> Result<(), vush_http::BuildError> {
//! let executor = Executor::new(&HttpSettings::default())?;
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

mod body;
mod classify;
mod connector;
mod executor;
mod follow;
mod json;
mod prepare;
mod stage;
mod tls;

pub use executor::{BuildError, Executor, Session};
