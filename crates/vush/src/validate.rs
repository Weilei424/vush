//! The `validate` subcommand: loads each file and renders the result.

use std::io::Write;
use std::path::PathBuf;

use vush_config::{ConfigError, MAX_ERRORS};

/// Closing note for `vush validate --help`.
pub(crate) const AFTER_HELP: &str = "Some rules run only after a file has no other errors, so fixing one round of problems can reveal more.\n\nExit status: 0 if every file is valid, 2 if any file is not.";

/// Checks each file in order. Returns true when every file is valid.
/// Write failures are ignored so a closed pipe never panics.
pub(crate) fn run(files: &[PathBuf], out: &mut dyn Write, err: &mut dyn Write) -> bool {
    let mut all_valid = true;
    for path in files {
        let file = path.display().to_string();
        match vush_config::load(path) {
            Ok(_) => {
                let _ = writeln!(out, "{}: valid", printable(&file));
            }
            Err(errors) => {
                all_valid = false;
                report(&file, &errors, err);
            }
        }
    }
    all_valid
}

/// Writes one block per problem, then the file's status line.
fn report(file: &str, errors: &[ConfigError], err: &mut dyn Write) {
    let file = printable(file);
    for error in errors.iter().take(MAX_ERRORS) {
        let _ = writeln!(err, "error: {}", printable(&error.message));
        match error.location {
            Some(at) => {
                let _ = writeln!(err, "  --> {file}:{}:{}", at.line, at.column);
            }
            None => {
                let _ = writeln!(err, "  --> {file}");
            }
        }
        if !error.path.is_empty() {
            let _ = writeln!(err, "   = at {}", printable(&error.path));
        }
        let _ = writeln!(err);
    }
    if errors.len() > MAX_ERRORS {
        let _ = writeln!(
            err,
            "{file}: not valid, showing the first {MAX_ERRORS} problems"
        );
    } else if errors.len() == 1 {
        let _ = writeln!(err, "{file}: not valid, 1 problem");
    } else {
        let _ = writeln!(err, "{file}: not valid, {} problems", errors.len());
    }
}

/// Escapes control characters so no printed line can move the cursor or
/// recolor the terminal.
fn printable(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            escaped.extend(c.escape_default());
        } else {
            escaped.push(c);
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use vush_config::Location;

    fn problem(path: &str, at: Option<(u32, u32)>, message: &str) -> ConfigError {
        ConfigError {
            path: path.to_string(),
            location: at.map(|(line, column)| Location { line, column }),
            message: message.to_string(),
        }
    }

    fn rendered(file: &str, errors: &[ConfigError]) -> String {
        let mut out = Vec::new();
        report(file, errors, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_block_shows_every_part_it_has() {
        let full = problem("a.b", Some((3, 7)), "bad");
        assert_eq!(
            rendered("t.yaml", &[full]),
            "error: bad\n  --> t.yaml:3:7\n   = at a.b\n\nt.yaml: not valid, 1 problem\n"
        );
        let no_path = problem("", Some((3, 7)), "bad");
        assert_eq!(
            rendered("t.yaml", &[no_path]),
            "error: bad\n  --> t.yaml:3:7\n\nt.yaml: not valid, 1 problem\n"
        );
        let no_location = problem("", None, "bad");
        assert_eq!(
            rendered("t.yaml", &[no_location]),
            "error: bad\n  --> t.yaml\n\nt.yaml: not valid, 1 problem\n"
        );
    }

    #[test]
    fn the_status_line_counts_problems() {
        let two = [problem("", None, "a"), problem("", None, "b")];
        assert!(rendered("t", &two).ends_with("t: not valid, 2 problems\n"));
        let cap = vec![problem("", None, "x"); MAX_ERRORS];
        assert!(rendered("t", &cap).ends_with("t: not valid, 100 problems\n"));
    }

    #[test]
    fn past_the_cap_the_closing_note_becomes_the_status_line() {
        let mut list = vec![problem("", None, "x"); MAX_ERRORS];
        list.push(problem(
            "",
            None,
            "too many errors, only the first 100 are shown",
        ));
        let text = rendered("t", &list);
        assert_eq!(text.matches("error: ").count(), MAX_ERRORS);
        assert!(!text.contains("too many errors"));
        assert!(text.ends_with("t: not valid, showing the first 100 problems\n"));
    }

    #[test]
    fn control_characters_are_escaped_everywhere() {
        let text = rendered(
            "a\nb.yaml",
            &[problem("", Some((1, 1)), "bad \u{1b}[31m char")],
        );
        assert!(text.contains("a\\nb.yaml:1:1"));
        assert!(text.contains("bad \\u{1b}[31m char"));
        assert!(text.lines().all(|line| !line.chars().any(char::is_control)));
    }

    #[test]
    fn other_text_is_left_alone() {
        assert_eq!(printable("café \"x\"\\y.yaml"), "café \"x\"\\y.yaml");
    }

    #[test]
    fn a_failing_writer_changes_nothing() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        assert!(!run(
            &[PathBuf::from("no-such-file.yaml")],
            &mut Broken,
            &mut Broken
        ));
    }
}
