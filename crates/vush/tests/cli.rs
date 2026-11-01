use std::process::{Command, Output};

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
