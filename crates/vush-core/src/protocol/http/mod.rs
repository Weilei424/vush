//! HTTP at the protocol boundary.
//!
//! It holds the [`HttpRequest`] that core resolves, the [`HttpResponse`] an
//! executor returns, and the [`HttpExecutor`] trait that `vush-http`
//! implements.

mod executor;
mod request;
mod response;

pub use executor::HttpExecutor;
pub use request::{HttpBody, HttpRequest, RequestUrl};
pub use response::{HttpHop, HttpResponse, WireVersion};
