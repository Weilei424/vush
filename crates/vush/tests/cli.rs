use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Runs the built `vush` binary. CLICOLOR_FORCE is removed so piped output
/// stays uncolored on every machine.
fn vush(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vush"))
        .args(args)
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

fn expected_version() -> String {
    format!("vush {}\n", env!("CARGO_PKG_VERSION"))
}

#[test]
fn help_flag_prints_usage_and_exits_zero() {
    let out = vush(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("Usage: vush"));
    assert!(
        !stdout(&out).contains('\u{1b}'),
        "help must not contain ANSI escapes when piped"
    );
}

#[test]
fn version_flag_prints_package_version() {
    let out = vush(&["--version"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), expected_version());
}

#[test]
fn short_flags_match_long_flags() {
    let h = vush(&["-h"]);
    assert_eq!(h.status.code(), Some(0));
    assert!(stdout(&h).contains("Usage: vush"));
    let v = vush(&["-V"]);
    assert_eq!(v.status.code(), Some(0));
    assert_eq!(stdout(&v), expected_version());
}

#[test]
fn no_arguments_prints_help_to_stderr_and_exits_two() {
    let out = vush(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("Usage: vush"));
}

#[test]
fn unknown_flag_is_rejected_with_exit_two() {
    let out = vush(&["--bogus"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("--bogus"));
}

#[test]
fn subcommand_like_argument_is_rejected() {
    let out = vush(&["run", "test.yaml"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("'run'"));
}

const VALID_YAML: &str = "schema_version: 1\nname: smoke\nscenarios:\n  - name: ping\n    load: {model: closed_vus, vus: 1, iterations: 1}\n    steps:\n      - request: {method: GET, url: \"http://localhost:8080/\"}\n";
const VALID_JSON: &str = r#"{"schema_version": 1, "name": "smoke", "scenarios": [{"name": "ping", "load": {"model": "closed_vus", "vus": 1, "iterations": 1}, "steps": [{"request": {"method": "GET", "url": "http://localhost:8080/"}}]}]}"#;
const ONE_PROBLEM: &str = "schema_version: 1\nname: smoke\n";

/// A fresh directory for one test. `name` must be unique across tests.
fn dir(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("failed to create test directory");
    path
}

/// Runs `vush validate` in `dir` with these file arguments.
fn validate<S: AsRef<OsStr>>(dir: &Path, files: &[S]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vush"))
        .arg("validate")
        .args(files)
        .current_dir(dir)
        .env_remove("CLICOLOR_FORCE")
        .output()
        .expect("failed to run vush binary")
}

fn write(dir: &Path, name: &str, text: impl AsRef<[u8]>) {
    fs::write(dir.join(name), text).expect("failed to write fixture");
}

#[test]
fn validate_accepts_valid_yaml_and_json() {
    let d = dir("validate_accepts_valid_yaml_and_json");
    write(&d, "ok.yaml", VALID_YAML);
    write(&d, "ok.json", VALID_JSON);
    let out = validate(&d, &["ok.yaml", "ok.json"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "ok.yaml: valid\nok.json: valid\n");
    assert_eq!(stderr(&out), "");
}

#[test]
fn validate_prints_each_problem_as_a_block() {
    let d = dir("validate_prints_each_problem_as_a_block");
    write(
        &d,
        "bad.yaml",
        "schema_version: 1\nname: smoke\nbase_ur: http://localhost\nscenarios:\n  - name: ping\n    load: {model: closed_vus, vus: 1, iterations: 1}\n    steps:\n      - wait: 5\n",
    );
    let out = validate(&d, &["bad.yaml"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stdout(&out), "");
    assert_eq!(
        stderr(&out),
        "error: unknown key `base_ur`, did you mean `base_url`?\n  --> bad.yaml:3:1\n   = at base_ur\n\nerror: expected a duration such as 500ms, 5s, 5m or 1h30m, found a number\n  --> bad.yaml:8:15\n   = at scenarios[0].steps[0].wait\n\nbad.yaml: not valid, 2 problems\n"
    );
}

#[test]
fn validate_shows_a_syntax_error_without_a_key_path() {
    let d = dir("validate_shows_a_syntax_error_without_a_key_path");
    write(&d, "broken.yaml", "a: [1, 2\n");
    let out = validate(&d, &["broken.yaml"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert_eq!(err.lines().nth(1), Some("  --> broken.yaml:2:1"));
    assert!(!err.contains("= at"));
    assert!(err.ends_with("\n\nbroken.yaml: not valid, 1 problem\n"));
}

#[test]
fn validate_reports_a_missing_file_without_a_location() {
    let d = dir("validate_reports_a_missing_file_without_a_location");
    let out = validate(&d, &["missing.yaml"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.starts_with("error: can't read the file: "));
    assert!(err.ends_with("\n  --> missing.yaml\n\nmissing.yaml: not valid, 1 problem\n"));
}

#[test]
fn validate_reports_an_unsupported_extension() {
    let d = dir("validate_reports_an_unsupported_extension");
    let out = validate(&d, &["t.toml"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        stderr(&out),
        "error: unknown file type; vush reads .yaml, .yml and .json files\n  --> t.toml\n\nt.toml: not valid, 1 problem\n"
    );
}

#[test]
fn validate_keeps_checking_after_a_bad_file() {
    let d = dir("validate_keeps_checking_after_a_bad_file");
    write(&d, "ok.yaml", VALID_YAML);
    write(&d, "bad.yaml", ONE_PROBLEM);
    write(&d, "ok.json", VALID_JSON);
    let out = validate(&d, &["ok.yaml", "bad.yaml", "ok.json"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stdout(&out), "ok.yaml: valid\nok.json: valid\n");
    assert!(stderr(&out).ends_with("bad.yaml: not valid, 1 problem\n"));
}

#[test]
fn validate_needs_at_least_one_file() {
    let out = vush(&["validate"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("<FILE>"));
}

#[test]
fn validate_help_mentions_later_rules_and_exit_status() {
    let out = vush(&["validate", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("fixing one round of problems"));
    assert!(stdout(&out).contains("Exit status: 0"));
}

#[test]
fn validate_caps_the_problems_it_prints() {
    let d = dir("validate_caps_the_problems_it_prints");
    let mut text = String::from(VALID_YAML);
    for i in 0..150 {
        text.push_str(&format!("zz{i}: 1\n"));
    }
    write(&d, "many.yaml", text);
    let out = validate(&d, &["many.yaml"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert_eq!(err.matches("error: ").count(), 100);
    assert!(!err.contains("too many errors"));
    assert!(err.ends_with("many.yaml: not valid, showing the first 100 problems\n"));
}

#[test]
fn validate_rejects_an_oversized_file() {
    let d = dir("validate_rejects_an_oversized_file");
    write(&d, "huge.yaml", vec![b' '; 10 * 1024 * 1024 + 1]);
    let out = validate(&d, &["huge.yaml"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        stderr(&out),
        "error: the file is larger than the limit of 10 MiB\n  --> huge.yaml\n\nhuge.yaml: not valid, 1 problem\n"
    );
}

#[test]
fn validate_survives_a_closed_stdout() {
    let d = dir("validate_survives_a_closed_stdout");
    write(&d, "ok.yaml", VALID_YAML);
    let (reader, writer) = std::io::pipe().expect("failed to make a pipe");
    drop(reader);
    let child = Command::new(env!("CARGO_BIN_EXE_vush"))
        .args(["validate", "ok.yaml"])
        .current_dir(&d)
        .stdout(writer)
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run vush binary");
    let out = child.wait_with_output().expect("failed to wait for vush");
    assert_eq!(out.status.code(), Some(0));
    assert!(!stderr(&out).contains("panicked"));
}

#[test]
fn validate_never_prints_values_from_the_file() {
    let d = dir("validate_never_prints_values_from_the_file");
    write(
        &d,
        "valid.yaml",
        "schema_version: 1\nname: hunter2-name\nscenarios:\n  - name: hunter2-scenario\n    load: {model: closed_vus, vus: 1, iterations: 1}\n    steps:\n      - request:\n          method: GET\n          url: \"http://localhost/hunter2\"\n          headers: {Authorization: \"Bearer hunter2\"}\n          body: \"hunter2\"\n",
    );
    write(
        &d,
        "broken.yaml",
        "schema_version: 1\nname: hunter2-name\nscenarios:\n  - name: hunter2-scenario\n    load: {model: closed_vus, vus: 1, duration: hunter2}\n    steps:\n      - request:\n          method: GET\n          url: \"http://localhost/hunter2${\"\n          headers: {Authorization: \"Bearer hunter2${\"}\n          body: \"hunter2${\"\n          timeout: hunter2\n",
    );
    for (file, code) in [("valid.yaml", 0), ("broken.yaml", 2)] {
        let out = validate(&d, &[file]);
        assert_eq!(out.status.code(), Some(code));
        assert!(!stdout(&out).contains("hunter2"));
        assert!(!stderr(&out).contains("hunter2"));
    }
}

#[test]
fn validate_keeps_file_order_when_streams_share_a_destination() {
    let d = dir("validate_keeps_file_order_when_streams_share_a_destination");
    write(&d, "ok.yaml", VALID_YAML);
    write(&d, "bad.yaml", ONE_PROBLEM);
    write(&d, "ok.json", VALID_JSON);
    let merged = d.join("merged.txt");
    let file = fs::File::create(&merged).expect("failed to create merged file");
    let status = Command::new(env!("CARGO_BIN_EXE_vush"))
        .args(["validate", "ok.yaml", "bad.yaml", "ok.json"])
        .current_dir(&d)
        .stdout(file.try_clone().expect("failed to clone file"))
        .stderr(file)
        .status()
        .expect("failed to run vush binary");
    assert_eq!(status.code(), Some(2));
    assert_eq!(
        fs::read_to_string(&merged).expect("failed to read merged file"),
        "ok.yaml: valid\nerror: missing required key `scenarios`\n  --> bad.yaml:1:1\n\nbad.yaml: not valid, 1 problem\nok.json: valid\n"
    );
}

#[test]
fn validate_reports_a_directory_as_not_valid() {
    let d = dir("validate_reports_a_directory_as_not_valid");
    fs::create_dir(d.join("tests")).expect("failed to create directory");
    fs::create_dir(d.join("d.yaml")).expect("failed to create directory");
    let out = validate(&d, &["tests"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("error: unknown file type"));
    assert!(stderr(&out).contains("  --> tests\n"));
    let out = validate(&d, &["d.yaml"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).starts_with("error: can't read the file: "));
}

#[test]
fn validate_handles_a_non_utf8_file_name() {
    let d = dir("validate_handles_a_non_utf8_file_name");
    let out = validate(&d, &[OsStr::from_bytes(b"caf\xe9.yaml")]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("caf\u{fffd}.yaml"));
    assert!(stderr(&out).contains("not valid"));
    assert!(!stderr(&out).contains("panicked"));
}

#[test]
fn validate_reports_empty_and_non_utf8_files_in_block_form() {
    let d = dir("validate_reports_empty_and_non_utf8_files_in_block_form");
    write(&d, "empty.yaml", "");
    write(&d, "binary.yaml", b"name: hunter2\xff\n");
    let out = validate(&d, &["empty.yaml", "binary.yaml"]);
    assert_eq!(
        stderr(&out),
        "error: the file is empty\n  --> empty.yaml\n\nempty.yaml: not valid, 1 problem\nerror: the file isn't valid UTF-8\n  --> binary.yaml:1:14\n\nbinary.yaml: not valid, 1 problem\n"
    );
}
