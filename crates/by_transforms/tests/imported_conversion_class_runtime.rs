//! Runtime test for the names a module imports the target class of a cross-module conversion
//! under.
//!
//! A conversion to a class declared in another module imports that class under an alias, so
//! that it never rebinds a name the module means something else by. Two classes of one name
//! from two modules are two targets, and a module can spell a name the alias would otherwise
//! take; either way a shared alias would send a conversion through the wrong class, or through
//! something that is not a class at all, with the checker satisfied.
//!
//! The lowering needs cross-module type information, so the project is built through a typed db
//! rather than the single-file `transpile`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile_typed};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
use ty_project::{ProjectMetadata, TestDb};

mod interpreters;

const METRIC: &str = r#"
frozen data class Length:
    value: float

    class def __of__(cls, value: int) -> Length:
        return Length(float(value * 1000))
"#;

const IMPERIAL: &str = r#"
frozen data class Length:
    value: float

    class def __of__(cls, value: int) -> Length:
        return Length(float(value * 12))
"#;

/// converts to both `Length`s, and spells the name the metric one's alias would begin with
const MAIN: &str = r#"
import metric
import imperial

_by_conv__metric__Length = "spelled"

def in_millimetres(amount: metric.Length) -> float:
    return amount.value

def in_inches(amount: imperial.Length) -> float:
    return amount.value

assert in_millimetres(2) == 2000.0, "the metric conversion"
assert in_inches(2) == 24.0, "the imperial conversion"
assert _by_conv__metric__Length == "spelled"
print("ok")
"#;

const FILES: &[(&str, &str)] = &[
    ("/metric.by", METRIC),
    ("/imperial.by", IMPERIAL),
    ("/main.by", MAIN),
];

/// transpile every file of the project for `target` into a fresh directory under the cargo
/// temp dir
fn build(target: PythonVersion) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("imported_conversion_class");
    // a stale directory from an earlier run would mask a transpile failure
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create case dir");

    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/"),
    ));
    db.init_program().expect("program init failed");
    for (path, source) in FILES {
        db.write_file(path, source).expect("write file failed");
    }
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    for (path, _) in FILES {
        let file = system_path_to_file(&db, path).expect("file not in db");
        let transpiled = transpile_typed(&db, file, &config, None)
            .unwrap_or_else(|error| panic!("transpile of {path} should succeed: {error}"));
        let relative = path.trim_start_matches('/').replace(".by", ".py");
        fs::write(dir.join(relative), transpiled).expect("write module");
    }
    dir
}

#[test]
fn a_conversion_target_is_imported_clear_of_every_other_name() {
    let target = PythonVersion::PY310;
    let Some(interpreter) = interpreters::oldest(target, "", "") else {
        return;
    };
    let dir = build(target);
    let output = Command::new(&interpreter.command)
        .arg("main.py")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled program failed on {interpreter}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- main.py ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(dir.join("main.py")).unwrap_or_default(),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(&interpreter, target);
}
