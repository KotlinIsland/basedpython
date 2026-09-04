//! Runtime divergence test for a literal conversion whose target class is
//! reached through a package re-export.
//!
//! `Dp` is declared in `ui.geometry` and re-exported by `ui/__init__`
//! (`from .geometry export Dp`); the program only imports `ui`. The checker
//! accepts `pad(8)` through `Dp.__of__`, and the lowering has to import `Dp`
//! from its declaring module under the conversion alias — spelled through the
//! package the file does import. The transform unit tests fix that text; this
//! test proves the emitted import actually resolves on a real interpreter,
//! with the package laid out on disk the way the checker resolved it.
//!
//! The lowering needs cross-module type information, so the project is built
//! through a typed db rather than the single-file `transpile`. No third-party
//! packages are needed; if no `python3` is found the test skips rather than
//! fails.

#![expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile_typed};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
use ty_project::{ProjectMetadata, TestDb};

mod common;
use common::python;

const PACKAGE_INIT: &str = "from .geometry export Dp\n";

const GEOMETRY: &str = r#"
frozen data class Dp:
    value: float

    class def __of__(cls, value: int | float) -> Dp:
        return Dp(float(value))
"#;

/// imports `Dp` through the package only, and converts a literal at a call
/// argument, an annotated assignment and a return — every conversion site
/// reaches `Dp` the same way
const MAIN: &str = r#"
from ui import Dp

def pad(amount: Dp) -> float:
    return amount.value

def default() -> Dp:
    return 4

gap: Dp = 2
assert pad(8) == 8.0, pad(8)
assert gap.value == 2.0, gap
assert default().value == 4.0, default()
print("ok")
"#;

/// the project's files, in the layout the checker resolves them under
const FILES: &[(&str, &str)] = &[
    ("/ui/__init__.by", PACKAGE_INIT),
    ("/ui/geometry.by", GEOMETRY),
    ("/main.by", MAIN),
];

fn build_db() -> TestDb {
    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/"),
    ));
    db.init_program().expect("program init failed");
    for (path, source) in FILES {
        db.write_file(path, source).expect("write file failed");
    }
    db
}

/// transpile every file of the project through the typed pipeline into a fresh
/// directory under the cargo temp dir, keeping the package layout
fn build_case(case: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(case);
    // a stale directory from an earlier run would mask a transpile failure
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("ui")).expect("create case dir");

    let db = build_db();
    let config = Config {
        min_version: PythonVersion::PY313,
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
fn conversion_through_a_re_export_resolves_at_runtime() {
    let Some(python) = python() else {
        eprintln!("skipping re-export conversion runtime test: no `python3` interpreter found");
        return;
    };
    let dir = build_case("reexport_conversion");

    let output = Command::new(&python)
        .arg("main.py")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
