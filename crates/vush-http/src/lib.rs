//! HTTP protocol executor for Vush.
//!
//! This crate will implement `vush_core::protocol::http::HttpExecutor` for
//! HTTP/1.1, HTTP/2 and TLS. Each `send` makes exactly one request on the
//! wire, or none when it fails in `prepare`. It never follows a redirect and
//! never retries, because core's driver owns both loops. This crate owns the
//! redirect rules (`follow`), connection pooling, per-hop timeouts, response
//! bounds, and the per-user session that holds cookies. It must not schedule
//! work or own global counters: scheduling belongs to `vush-core`, and
//! aggregation to the metrics layer.

// Nothing outside the tests calls these until `send` lands (plan Task 6), and
// unused code is a warning. Task 6 drops the `cfg(test)`.
#[cfg(test)]
mod connector;
#[cfg(test)]
mod follow;
#[cfg(test)]
mod json;
#[cfg(test)]
mod prepare;
#[cfg(test)]
mod stage;
#[cfg(test)]
mod tls;
