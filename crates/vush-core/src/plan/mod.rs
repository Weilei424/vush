//! The typed workload plan and the value types it is built from.
//!
//! Every text form the test-file schema accepts is parsed here, with no
//! dependencies, into a typed value, so the engine never interprets text at
//! request time.

mod json_path;
mod template;
mod value;

pub use json_path::{JsonPath, JsonPathSegment};
pub use template::{Template, TemplatePart, VarRef};
pub use value::{ByteSize, JsonNumber, Rate, RateUnit, Ratio, ValueError, parse_duration};
