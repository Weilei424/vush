//! HTTP protocol executor for Vush.
//!
//! This crate will implement `vush_core::protocol::http::HttpExecutor` for
//! HTTP/1.1, HTTP/2 and TLS. Each `send` makes exactly one request on the
//! wire: it never follows a redirect and never retries, because core's
//! driver owns both loops. This crate owns the redirect rules (`follow`),
//! connection pooling, per-hop timeouts, response bounds, and the per-user
//! session that holds cookies. It must not schedule work or own global
//! counters: scheduling belongs to `vush-core`, and aggregation to the
//! metrics layer.
