//! The files in `examples/` are what a new user copies first, so each YAML
//! example needs a JSON twin that builds the same plan.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use vush_config::{Format, load};

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// The sorted names of the files in `examples/` that have a config extension.
fn example_files() -> Vec<String> {
    let dir = examples_dir();
    let entries =
        fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    let mut names: Vec<String> = entries
        .map(|entry| entry.expect("directory entry").file_name())
        .filter(|name| Format::from_path(Path::new(name)).is_some())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The files grouped by stem, each stem holding its `(yaml, json)` file names.
fn pairs() -> BTreeMap<String, (Vec<String>, Vec<String>)> {
    let mut map: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for name in example_files() {
        let path = Path::new(&name);
        let stem = path
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .into_owned();
        let entry = map.entry(stem).or_default();
        match Format::from_path(path) {
            Some(Format::Yaml) => entry.0.push(name),
            Some(Format::Json) => entry.1.push(name),
            None => unreachable!("example_files keeps only config files"),
        }
    }
    map
}

fn load_example(name: &str) -> vush_core::plan::WorkloadPlan {
    let path = examples_dir().join(name);
    load(&path).unwrap_or_else(|errors| {
        let lines: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
        panic!("{name} failed to load:\n{}", lines.join("\n"))
    })
}

#[test]
fn examples_folder_holds_the_six_examples() {
    assert_eq!(
        example_files(),
        [
            "flow.json",
            "flow.yaml",
            "rate.json",
            "rate.yaml",
            "smoke.json",
            "smoke.yaml"
        ]
    );
}

#[test]
fn examples_come_in_yaml_and_json_pairs() {
    for (stem, (yaml, json)) in pairs() {
        assert_eq!(yaml.len(), 1, "{stem}: expected exactly one YAML file");
        assert_eq!(json.len(), 1, "{stem}: expected exactly one JSON file");
    }
}

#[test]
fn each_example_builds_the_same_plan_from_yaml_and_json() {
    for (stem, (yaml, json)) in pairs() {
        let yaml_plan = load_example(&yaml[0]);
        let json_plan = load_example(&json[0]);
        assert_eq!(yaml_plan, json_plan, "{stem}");
    }
}
