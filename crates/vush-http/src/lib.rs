//! HTTP protocol executor for Vush.
//!
//! This crate will implement the protocol-neutral executor interface for
//! HTTP/1.1, HTTP/2 and TLS, including connection pooling, timeouts and
//! opt-in retries. It must not schedule work or own global counters:
//! scheduling belongs to `vush-core`, and aggregation to the metrics layer.
