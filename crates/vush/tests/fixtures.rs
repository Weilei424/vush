//! `vush validate` on the committed examples and on the schema doc's complete
//! example.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use vush_config::Format;

const SCHEMA_DOC: &str = include_str!("../../../docs/config-schema.md");

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn examples_dir() -> PathBuf {
    manifest_dir().join("../../examples")
}

/// The sorted names of the files in `dir` that `Format::from_path` accepts.
fn test_files(dir: &Path) -> Vec<String> {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read folder {}: {e}", dir.display()));
    let mut names: Vec<String> = entries
        .map(|entry| entry.expect("directory entry").file_name())
        .filter(|name| Format::from_path(Path::new(name)).is_some())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Runs `vush validate` in `dir` with these file arguments. CLICOLOR_FORCE is
/// removed so piped output stays uncolored on every machine.
fn validate_in<S: AsRef<OsStr>>(dir: &Path, files: &[S]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vush"))
        .arg("validate")
        .args(files)
        .current_dir(dir)
        .env_remove("CLICOLOR_FORCE")
        .output()
        .expect("failed to run vush binary")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The first `lang` code block after the "Complete example" heading. It mirrors
/// `example_block` in `crates/vush-config/tests/common/mod.rs`, with the same
/// extraction rule.
fn example_block(lang: &str) -> &'static str {
    let tail = &SCHEMA_DOC[SCHEMA_DOC.find("## Complete example").expect("heading")..];
    let open = format!("```{lang}\n");
    let start = tail.find(&open).expect("opening fence") + open.len();
    let end = tail[start..].find("\n```").expect("closing fence");
    &tail[start..=start + end]
}

#[test]
fn examples_pass_validate() {
    let files = test_files(&examples_dir());
    assert_eq!(files.len(), 6, "{files:?}");
    let out = validate_in(&examples_dir(), &files);
    let expected: String = files.iter().map(|f| format!("{f}: valid\n")).collect();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), expected);
    assert_eq!(stderr(&out), "");
}

#[test]
fn schema_doc_complete_example_passes_validate() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("complete-example");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("failed to create test directory");
    fs::write(dir.join("complete.yaml"), example_block("yaml")).expect("failed to write fixture");
    fs::write(dir.join("complete.json"), example_block("json")).expect("failed to write fixture");
    let out = validate_in(&dir, &["complete.yaml", "complete.json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "complete.yaml: valid\ncomplete.json: valid\n");
    assert_eq!(stderr(&out), "");
}
