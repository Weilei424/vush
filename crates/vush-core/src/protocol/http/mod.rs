//! HTTP at the protocol boundary.
//!
//! It holds the request that core resolves, the `HttpResponse` an executor
//! returns, and the `HttpExecutor` trait that `vush-http` implements.

mod request;

pub use request::{HttpBody, HttpRequest, RequestUrl};
