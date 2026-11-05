//! `load` reads files from disk.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use vush_config::{Location, MAX_FILE_BYTES, load};

/// A file in the temp directory that is removed when dropped.
struct TempFile(PathBuf);

impl TempFile {
    fn new(name: &str, bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!("vush-config-{}-{name}", std::process::id()));
        fs::write(&path, bytes).unwrap();
        TempFile(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

const YAML: &str = "\
schema_version: 1
name: n
scenarios:
  - name: s
    load: {model: closed_vus, vus: 1, duration: 1s}
    steps: [{wait: 1s}]
";

const JSON: &str = r#"{"schema_version": 1, "name": "n", "scenarios": [
  {"name": "s", "load": {"model": "closed_vus", "vus": 1, "duration": "1s"},
   "steps": [{"wait": "1s"}]}]}"#;

#[test]
fn every_extension_loads() {
    for (name, text) in [
        ("ext1.yaml", YAML),
        ("ext2.yml", YAML),
        ("ext3.YAML", YAML),
        ("ext4.json", JSON),
        ("ext5.JSON", JSON),
    ] {
        let file = TempFile::new(name, text.as_bytes());
        let plan = load(file.path()).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(plan.name(), "n", "{name}");
    }
}

#[test]
fn an_unknown_extension_is_refused_before_reading() {
    for path in ["/nonexistent/x.toml", "/nonexistent/x"] {
        let errors = load(Path::new(path)).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains(".yaml, .yml and .json"));
        assert_eq!(errors[0].location, None);
    }
}

#[test]
fn a_missing_file_is_reported() {
    let errors = load(Path::new("/nonexistent/x.yaml")).unwrap_err();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("can't read the file"));
    assert_eq!(errors[0].location, None);
}

#[test]
fn invalid_utf8_reports_its_location() {
    let file = TempFile::new("utf8.yaml", b"a: 1\nb: \xff\n");
    let errors = load(file.path()).unwrap_err();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("isn't valid UTF-8"));
    assert_eq!(errors[0].location, Some(Location { line: 2, column: 4 }));
}

#[test]
fn a_file_over_the_limit_is_refused() {
    let file = TempFile::new("over.json", b"");
    File::options()
        .write(true)
        .open(file.path())
        .unwrap()
        .set_len(MAX_FILE_BYTES as u64 + 1)
        .unwrap();
    let errors = load(file.path()).unwrap_err();
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("larger than the limit of 10 MiB")
    );
    assert_eq!(errors[0].location, None);
}

#[test]
fn a_file_at_the_limit_is_read() {
    let file = TempFile::new("at.json", b"");
    File::options()
        .write(true)
        .open(file.path())
        .unwrap()
        .set_len(MAX_FILE_BYTES as u64)
        .unwrap();
    let errors = load(file.path()).unwrap_err();
    assert!(!errors.is_empty());
    assert!(
        errors
            .iter()
            .all(|e| !e.message.contains("larger than the limit"))
    );
}
