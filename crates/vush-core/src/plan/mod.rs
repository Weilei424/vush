//! The typed workload plan and the value types it is built from.
//!
//! Every text form the test-file schema accepts (durations, rates, sizes and
//! percentages today) is parsed here, with no dependencies, into a typed value,
//! so the engine never interprets text at request time.

mod value;

pub use value::{ByteSize, JsonNumber, Rate, RateUnit, Ratio, ValueError, parse_duration};
