//! Runtime tests for static resource imports.
//!
//! The point of the feature is that `config.a.b[1]` has a type *and* a value,
//! and that they are the same thing. The mdtests hold up the type half; only a
//! real interpreter holds up the other. Every assertion below is written so
//! that a rendering which type checked but did not run — a tuple missing its
//! trailing comma, a helper class named before it was defined, a string whose
//! escaping did not survive — fails here.

#![expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use by_transforms::{Config, transpile_typed};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
use ty_project::{ProjectMetadata, TestDb};

mod common;

/// transpile `/proj/main.by` with `files` written around it, into a fresh
/// directory under the cargo temp dir.
fn build_case(case: &str, files: &[(&str, &str)]) -> PathBuf {
    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/proj"),
    ));
    for (path, source) in files {
        db.write_file(path, source).expect("write file failed");
    }
    db.init_program().expect("program init failed");

    let file = system_path_to_file(&db, "/proj/main.by").expect("file not in db");
    let transpiled =
        transpile_typed(&db, file, &Config::default(), None).expect("transpile should succeed");

    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(case);
    // a stale directory from an earlier run would mask a transpile failure
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create case dir");
    fs::write(dir.join("main.py"), transpiled).expect("write module");
    dir
}

/// run `main.py` in `dir`, asserting it exits cleanly and prints `ok`
fn run_main(python: &str, dir: &Path) {
    let output = Command::new(python)
        .arg("main.py")
        .current_dir(dir)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled program failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

const YAML: &str = "\
name: ty
port: 8080
ratio: 0.5
debug: true
missing: ~
quote: 'he said \"hi\"'
nested:
    deep:
        - 1
        - 2
one:
    - 9
empty_list: []
empty_map: {}
";

const YAML_MAIN: &str = r#"
import "data/config.yaml" as config

assert config.name == "ty", "a string"
assert config.port == 8080, "an integer"
assert config.ratio == 0.5, "a float"
assert config.debug is True, "a boolean"
assert config.missing is None, "a null"
assert config.quote == 'he said "hi"', "a string holding a quote"
assert config.nested.deep == (1, 2), "a nested sequence"
assert config.nested.deep[1] == 2, "and an index into it"
assert config.one == (9,), "a one-element sequence is still a sequence"
assert config.empty_list == (), "an empty sequence"
assert [n for n in vars(config.empty_map) if not n.startswith("__")] == [], "an empty mapping holds nothing"
print("ok")
"#;

/// a mapping inside a sequence inside a mapping inside a sequence: every one of
/// those becomes a class defined beside the value that names it, and a class
/// body runs when it is defined, so an ordering mistake is a `NameError` here
const JSON: &str = r#"{
  "servers": [
    { "host": "a", "tags": [{ "name": "x" }] },
    { "host": "b", "tags": [] }
  ],
  "build-backend": "left out",
  "root": "."
}"#;

const JSON_MAIN: &str = r#"
import "data/config.json" as config

assert config.servers[0].host == "a", "a mapping in a sequence"
assert config.servers[1].host == "b", "and the one after it"
assert config.servers[0].tags[0].name == "x", "a mapping in a sequence in a mapping in a sequence"
assert config.servers[1].tags == (), "an empty sequence beside a full one"
assert config.root == ".", "a key beside one python cannot name"
assert not hasattr(config, "build-backend"), "a key python cannot name is left out"
print("ok")
"#;

const TOML: &str = "\
[server]
host = \"localhost\"
ports = [80, 443]

[[server.routes]]
path = \"/a\"

[[server.routes]]
path = \"/b\"
";

const TOML_MAIN: &str = r#"
import "data/config.toml" as config

assert config.server.host == "localhost", "a table"
assert config.server.ports == (80, 443), "an array"
assert config.server.routes[1].path == "/b", "an array of tables"
print("ok")
"#;

/// the document is read where it is imported, so an import inside a function is
/// a class defined inside that function
const SCOPED_MAIN: &str = r#"
def load() -> str:
    import "data/config.json" as config
    return config.servers[0].tags[0].name


assert load() == "x", "a resource imported inside a function"
print("ok")
"#;

#[test]
fn a_yaml_document_reads_back_as_it_was_written() {
    let Some(python) = common::python() else {
        eprintln!("skipping static resource runtime test: no python interpreter found");
        return;
    };
    let dir = build_case(
        "resource_yaml",
        &[
            ("/proj/data/config.yaml", YAML),
            ("/proj/main.by", YAML_MAIN),
        ],
    );
    run_main(&python, &dir);
}

#[test]
fn a_mapping_nested_in_a_sequence_is_defined_before_it_is_named() {
    let Some(python) = common::python() else {
        eprintln!("skipping static resource runtime test: no python interpreter found");
        return;
    };
    let dir = build_case(
        "resource_json",
        &[
            ("/proj/data/config.json", JSON),
            ("/proj/main.by", JSON_MAIN),
        ],
    );
    run_main(&python, &dir);
}

#[test]
fn a_toml_document_reads_back_as_it_was_written() {
    let Some(python) = common::python() else {
        eprintln!("skipping static resource runtime test: no python interpreter found");
        return;
    };
    let dir = build_case(
        "resource_toml",
        &[
            ("/proj/data/config.toml", TOML),
            ("/proj/main.by", TOML_MAIN),
        ],
    );
    run_main(&python, &dir);
}

#[test]
fn a_resource_imported_inside_a_function_runs_there() {
    let Some(python) = common::python() else {
        eprintln!("skipping static resource runtime test: no python interpreter found");
        return;
    };
    let dir = build_case(
        "resource_scoped",
        &[
            ("/proj/data/config.json", JSON),
            ("/proj/main.by", SCOPED_MAIN),
        ],
    );
    run_main(&python, &dir);
}
