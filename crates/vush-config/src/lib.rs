//! Reads Vush test files written in YAML or JSON and builds a workload plan.
//! This module holds the shared pieces: located errors, the value tree both
//! readers produce, and the limits that keep hostile files bounded.

#![allow(dead_code)] // Removed in Task 8, once parse() uses everything.

mod error;
mod node;

pub use error::{ConfigError, Location};

/// Largest file `parse` and `load` accept.
pub const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
/// Levels of maps and lists. The top-level map is level 1.
pub const MAX_DEPTH: usize = 128;
/// Values in one file, counting alias copies.
pub const MAX_VALUES: usize = 250_000;
/// Bytes of string values and keys, counting alias copies.
pub const MAX_TEXT_BYTES: usize = 64 * 1024 * 1024;
/// Errors kept before the list is cut.
pub const MAX_ERRORS: usize = 100;
