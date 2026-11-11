//! `vush validate` on the committed examples and on the schema doc's complete
//! example.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
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

const BLESS: &str = "VUSH_BLESS=1 cargo test -p vush --test fixtures";

fn invalid_dir() -> PathBuf {
    manifest_dir().join("tests/invalid")
}

/// Bless mode rewrites goldens. Only the exact value `1` turns it on.
fn blessing() -> bool {
    std::env::var_os("VUSH_BLESS").is_some_and(|value| value == "1")
}

#[test]
fn invalid_files_print_their_golden_output() {
    let dir = invalid_dir();
    let files = test_files(&dir);
    assert!(
        files.len() >= 20,
        "found {} broken files in {}; expected at least 20",
        files.len(),
        dir.display()
    );
    let mut problems = Vec::new();
    for file in &files {
        let out = validate_in(&dir, &[file]);
        if out.status.code() != Some(2) {
            problems.push(format!(
                "{file}: exit status {:?}, expected 2",
                out.status.code()
            ));
            continue;
        }
        if !out.stdout.is_empty() {
            problems.push(format!(
                "{file}: expected nothing on stdout, got:\n{}",
                stdout(&out)
            ));
            continue;
        }
        let golden = dir.join(format!("{file}.stderr"));
        let want = fs::read(&golden).ok();
        if want.as_deref() == Some(&out.stderr[..]) {
            continue;
        }
        if blessing() {
            fs::write(&golden, &out.stderr).expect("failed to write a golden");
            // Writes straight to stderr so a passing bless run still lists the goldens it wrote (`eprintln!` output is captured).
            writeln!(std::io::stderr(), "wrote {file}.stderr").expect("failed to write to stderr");
        } else if let Some(want) = want {
            problems.push(format!(
                "{file}: stderr differs from {file}.stderr\n--- expected\n{}--- actual\n{}",
                String::from_utf8_lossy(&want),
                stderr(&out)
            ));
        } else {
            problems.push(format!(
                "{file}: {file}.stderr is missing; run {BLESS} to create it"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{}\n\nIf the new output is intended, run {BLESS} and review the diff.",
        problems.join("\n\n")
    );
}

#[test]
fn invalid_folder_holds_only_broken_files_and_goldens() {
    let dir = invalid_dir();
    let broken = test_files(&dir);
    let entries =
        fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read folder {}: {e}", dir.display()));
    let mut names: Vec<String> = entries
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    let mut problems = Vec::new();
    for name in &names {
        if broken.contains(name) {
            continue;
        }
        match name.strip_suffix(".stderr") {
            Some(stem) if broken.iter().any(|file| file == stem) => {}
            Some(_) => problems.push(format!("{name} has no broken file beside it; delete it")),
            None => problems.push(format!("{name} is neither a broken file nor a golden")),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn a_broken_file_fails_a_run_of_valid_examples() {
    let examples: Vec<PathBuf> = test_files(&examples_dir())
        .iter()
        .map(|name| examples_dir().join(name))
        .collect();
    assert_eq!(examples.len(), 6, "expected six examples");
    let broken = invalid_dir().join("unknown-key.yaml");
    let mut args = examples.clone();
    args.push(broken.clone());
    let out = validate_in(manifest_dir(), &args);
    assert_eq!(out.status.code(), Some(2), "stderr:\n{}", stderr(&out));
    let want: String = examples
        .iter()
        .map(|path| format!("{}: valid\n", path.display()))
        .collect();
    assert_eq!(stdout(&out), want);
    let tail = format!("{}: not valid, 1 problem\n", broken.display());
    assert!(stderr(&out).ends_with(&tail), "stderr:\n{}", stderr(&out));
}
