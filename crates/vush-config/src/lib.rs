//! Reads Vush test files written in YAML or JSON and builds a workload plan.
//!
//! Both formats are read into one located value tree, and one compile step
//! turns that tree into the plan. Every problem is reported with a key path
//! and a line and column, and no value from the file appears in a message.
//!
//! ```
//! use vush_config::{Format, parse};
//!
//! let yaml = r#"
//! schema_version: 1
//! name: smoke
//! scenarios:
//!   - name: ping
//!     load: { model: closed_vus, vus: 1, iterations: 1 }
//!     steps:
//!       - request: { method: GET, url: "http://localhost:8080/" }
//! "#;
//! let plan = parse(yaml, Format::Yaml).unwrap();
//! assert_eq!(plan.name(), "smoke");
//! ```

mod compile;
mod error;
mod json;
mod node;
mod scalar;
mod yaml;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use vush_core::plan::{PlanErrorKind, WorkloadPlan};

use crate::error::Errors;

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

/// The two file formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Yaml,
    Json,
}

impl Format {
    /// `.yaml`, `.yml` or `.json`, in any letter case. `None` for anything else.
    pub fn from_path(path: &Path) -> Option<Format> {
        let extension = path.extension()?.to_str()?;
        if extension.eq_ignore_ascii_case("yaml") || extension.eq_ignore_ascii_case("yml") {
            Some(Format::Yaml)
        } else if extension.eq_ignore_ascii_case("json") {
            Some(Format::Json)
        } else {
            None
        }
    }
}

const TOO_LARGE: &str = "the file is larger than the limit of 10 MiB";

/// Reads a test from text. The error list is never empty.
pub fn parse(text: &str, format: Format) -> Result<WorkloadPlan, Vec<ConfigError>> {
    let mut errors = Errors::new();
    if text.len() > MAX_FILE_BYTES {
        errors.push("", None, TOO_LARGE);
        return Err(errors.into_sorted());
    }
    let root = match format {
        Format::Yaml => yaml::read(text, &mut errors),
        Format::Json => json::read(text, &mut errors),
    };
    let Some(root) = root else {
        return Err(errors.into_sorted());
    };
    let draft = compile::compile(&root, &mut errors);
    if let (true, Some(draft)) = (errors.is_empty(), draft) {
        match WorkloadPlan::new(draft) {
            Ok(plan) => return Ok(plan),
            Err(plan_errors) => {
                for error in plan_errors {
                    // Whether the error is about a key, such as an `extract`
                    // name, so it points at the key rather than its value.
                    let (message, at_key) = match &error.kind {
                        // A new variant might hold file text; this match is where it gets checked.
                        kind @ (PlanErrorKind::NoScenarios
                        | PlanErrorKind::DuplicateScenarioName
                        | PlanErrorKind::NoSteps
                        | PlanErrorKind::NoStages
                        | PlanErrorKind::DurationTooLong
                        | PlanErrorKind::ZeroDuration
                        | PlanErrorKind::ZeroRate
                        | PlanErrorKind::IdleRamp
                        | PlanErrorKind::ZeroLengthRamp
                        | PlanErrorKind::TooFewAttempts
                        | PlanErrorKind::RetryNeedsOptIn
                        | PlanErrorKind::StatusOutOfRange
                        | PlanErrorKind::EmptyStatus
                        | PlanErrorKind::NoConditions
                        | PlanErrorKind::TemplateInName
                        | PlanErrorKind::PathNeedsBaseUrl
                        | PlanErrorKind::UnknownScenario
                        | PlanErrorKind::NonFiniteNumber
                        | PlanErrorKind::ControlCharacterInHeader
                        | PlanErrorKind::UnsupportedScheme
                        | PlanErrorKind::CredentialsInUrl
                        | PlanErrorKind::QueryInBaseUrl
                        | PlanErrorKind::UnsupportedMethod
                        | PlanErrorKind::VariableNotReadable { .. }
                        | PlanErrorKind::VariableNotSet { .. }) => (kind.to_string(), false),
                        kind @ (PlanErrorKind::InvalidVariableName
                        | PlanErrorKind::ReservedVariableName
                        | PlanErrorKind::InvalidHeaderName
                        | PlanErrorKind::ForbiddenHeader) => (kind.to_string(), true),
                        // Core dropped the rest, so the list gets the closing note.
                        PlanErrorKind::TooManyErrors => {
                            errors.mark_overflowed();
                            continue;
                        }
                    };
                    let at = compile::locate(&root, &error.path, at_key);
                    let path = compile::render_path(&error.path);
                    errors.push(&path, Some(at), message);
                }
            }
        }
    }
    Err(errors.into_sorted())
}

/// Reads a test from a file, choosing the format from its extension.
pub fn load(path: &Path) -> Result<WorkloadPlan, Vec<ConfigError>> {
    let mut errors = Errors::new();
    let Some(format) = Format::from_path(path) else {
        errors.push(
            "",
            None,
            "unknown file type; vush reads .yaml, .yml and .json files",
        );
        return Err(errors.into_sorted());
    };
    let mut bytes = Vec::new();
    let read = File::open(path)
        .and_then(|file| file.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes));
    if let Err(error) = read {
        errors.push("", None, format!("can't read the file: {error}"));
        return Err(errors.into_sorted());
    }
    if bytes.len() > MAX_FILE_BYTES {
        errors.push("", None, TOO_LARGE);
        return Err(errors.into_sorted());
    }
    match String::from_utf8(bytes) {
        Ok(text) => parse(&text, format),
        Err(error) => {
            let at = utf8_error_location(error.as_bytes(), error.utf8_error().valid_up_to());
            errors.push("", Some(at), "the file isn't valid UTF-8");
            Err(errors.into_sorted())
        }
    }
}

/// Where the first invalid byte sits: counts `\n`, `\r\n` and a lone `\r` as
/// line ends, characters as columns, and skips a leading byte-order mark.
fn utf8_error_location(bytes: &[u8], valid_up_to: usize) -> Location {
    let prefix = bytes
        .get(..valid_up_to)
        .and_then(|valid| std::str::from_utf8(valid).ok())
        .unwrap_or("");
    let prefix = prefix.strip_prefix('\u{feff}').unwrap_or(prefix);
    let (mut line, mut column) = (1u32, 1u32);
    let mut after_cr = false;
    for c in prefix.chars() {
        match c {
            '\r' => {
                line = line.saturating_add(1);
                column = 1;
            }
            '\n' if after_cr => {}
            '\n' => {
                line = line.saturating_add(1);
                column = 1;
            }
            _ => column = column.saturating_add(1),
        }
        after_cr = c == '\r';
    }
    Location { line, column }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_comes_from_the_extension() {
        for (name, want) in [
            ("a.yaml", Some(Format::Yaml)),
            ("a.yml", Some(Format::Yaml)),
            ("a.YAML", Some(Format::Yaml)),
            ("a.Yml", Some(Format::Yaml)),
            ("a.json", Some(Format::Json)),
            ("a.JSON", Some(Format::Json)),
            ("a.toml", None),
            ("a", None),
            (".yaml", None),
            ("a.yaml.bak", None),
        ] {
            assert_eq!(Format::from_path(Path::new(name)), want, "{name}");
        }
    }

    #[test]
    fn structural_problems_get_locations() {
        let text = "\
schema_version: 1
name: n
scenarios:
  - name: a
    load: {model: closed_vus, stages: []}
    steps: []
  - name: a
    load: {model: closed_vus, vus: 1, duration: 1s}
    steps:
      - repeat: {count: 1, steps: []}
";
        let errors = parse(text, Format::Yaml).unwrap_err();
        let found: Vec<_> = errors
            .iter()
            .map(|e| {
                (
                    e.path.as_str(),
                    e.location.unwrap().line,
                    e.location.unwrap().column,
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                ("scenarios[0].load.stages", 5, 39),
                ("scenarios[0].steps", 6, 12),
                ("scenarios[1].name", 7, 11),
                ("scenarios[1].steps[0].repeat.steps", 10, 35),
            ]
        );
        assert!(errors[0].message.contains("stage"));
        assert!(errors[1].message.contains("step"));
        assert!(errors[2].message.contains("already used"));
        assert!(errors[3].message.contains("step"));
    }

    #[test]
    fn an_empty_scenario_list_is_reported_at_the_list() {
        let errors =
            parse("schema_version: 1\nname: n\nscenarios: []\n", Format::Yaml).unwrap_err();
        assert_eq!(errors[0].path, "scenarios");
        assert_eq!(
            errors[0].location,
            Some(Location {
                line: 3,
                column: 12
            })
        );
    }

    #[test]
    fn extract_name_errors_point_at_the_name() {
        let text = "\
schema_version: 1
name: n
scenarios:
  - name: a
    load: {model: closed_vus, vus: 1, duration: 1s}
    steps:
      - request:
          method: GET
          extract:
            env: {header: X}
          url: http://localhost/
";
        let errors = parse(text, Format::Yaml).unwrap_err();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "scenarios[0].steps[0].request.extract.env");
        assert_eq!(
            errors[0].message,
            PlanErrorKind::ReservedVariableName.to_string()
        );
        assert_eq!(
            errors[0].location,
            Some(Location {
                line: 10,
                column: 13
            })
        );
    }

    #[test]
    fn oversized_text_is_refused_before_reading() {
        for format in [Format::Yaml, Format::Json] {
            let errors = parse(&" ".repeat(MAX_FILE_BYTES + 1), format).unwrap_err();
            assert_eq!(errors.len(), 1);
            assert!(
                errors[0]
                    .message
                    .contains("larger than the limit of 10 MiB")
            );
            assert_eq!(errors[0].location, None);
        }
    }

    #[test]
    fn reader_and_compile_errors_arrive_together_in_order() {
        let errors = parse(
            "schema_version: 1\nname: 010\nnmae: x\nscenarios: []\n",
            Format::Yaml,
        )
        .unwrap_err();
        let lines: Vec<_> = errors.iter().map(|e| e.location.unwrap().line).collect();
        assert_eq!(lines, [2, 3]);
        assert!(errors[0].message.contains("looks like a number"));
        assert!(errors[1].message.contains("unknown key `nmae`"));
    }

    /// Where `load` puts the first bad byte, as (line, column).
    fn locate(bytes: &[u8]) -> (u32, u32) {
        let valid_up_to = std::str::from_utf8(bytes)
            .expect_err("each case holds a bad byte")
            .valid_up_to();
        let at = utf8_error_location(bytes, valid_up_to);
        (at.line, at.column)
    }

    #[test]
    fn utf8_error_location_counts_lines_and_columns() {
        let cases: &[(&[u8], (u32, u32))] = &[
            (b"\xff", (1, 1)),                                   // bad first byte
            (b"ab\xff", (1, 3)),                                 // plain text
            (b"a\nb\xff", (2, 2)),                               // LF
            (b"a\rb\xff", (2, 2)),                               // lone CR
            (b"a\r\nb\xff", (2, 2)),                             // CRLF
            (b"a\r\r\nb\xff", (3, 2)),                           // CR then CRLF
            (b"a\n\rb\xff", (3, 2)),                             // LF then CR
            (b"a\r\xff", (2, 1)),                                // CR last
            (b"a\r\n\xff", (2, 1)),                              // CRLF last
            (b"\xef\xbb\xbfa\xff", (1, 2)),                      // leading mark
            (b"\xef\xbb\xbf\xff", (1, 1)),                       // mark alone
            (b"\xef\xbb\xbf\r\nb\xff", (2, 2)),                  // mark then CRLF
            (b"a\xef\xbb\xbf\xff", (1, 3)),                      // later mark is a character
            (b"\xc3\xa9\xff", (1, 2)),                           // 2-byte
            (b"\xe2\x82\xac\xff", (1, 2)),                       // 3-byte
            (b"\xf0\x9f\x98\x80\xff", (1, 2)),                   // 4-byte
            (b"ab\xc3", (1, 3)),                                 // cut off
            (b"a\n\xe2\x82", (2, 1)),                            // cut off after LF
            (b"\xef\xbb\xbf\xc3\xa9\r\xe2\x82\xac\xff", (2, 2)), // all together
        ];
        for (bytes, want) in cases {
            assert_eq!(locate(bytes), *want, "{bytes:?}");
        }
    }

    #[test]
    fn utf8_error_location_survives_a_bad_offset() {
        let start = Location { line: 1, column: 1 };
        assert_eq!(utf8_error_location(b"ab", 5), start);
        assert_eq!(utf8_error_location("é".as_bytes(), 1), start);
    }
}
