//! Runtime test for the imports an inferred annotation needs.
//!
//! An annotation the lowering writes can name a class the source never imported under that
//! name — `decimal.Decimal` for a value built by a `Decimal` imported as `Dec`. The module may
//! be one the source imports only for a checker, to keep it out of an import cycle, so where
//! python evaluates a class body's annotations as the class is made — from 3.10 until 3.14,
//! without `from __future__ import annotations` — the annotation is written as a string, and
//! only `typing.get_type_hints` evaluates it. Where the module's imports are deferred, the
//! module the annotation names is imported with them, and runs only when `get_type_hints`
//! asks; where they are not, it is imported for a checker alone.

use std::fs;
use std::path::Path;
use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile, transpile_typed};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
use ty_project::{ProjectMetadata, TestDb};

mod interpreters;
use interpreters::Interpreter;

const PROGRAM: &str = r#"
import typing
from decimal import Decimal as Dec

class A:
    d = Dec(1)

assert typing.get_type_hints(A)["d"] === type(A.d)
print("ok")
"#;

/// with imports not deferred, the module is imported for a checker alone, so the class is
/// made and the annotation stays the string it is written as
const PROGRAM_EAGER: &str = r#"
from decimal import Decimal as Dec

class A:
    d = Dec(1)

assert A.__annotations__["d"] == "decimal.Decimal"
print("ok")
"#;

fn run_at(
    interpreter: &Interpreter,
    target: PythonVersion,
    lazy_imports: bool,
    program: &str,
    annotation: &str,
) {
    let config = Config {
        min_version: target,
        lazy_imports,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");
    assert!(
        transpiled.contains(&format!("d: {annotation} = Dec(1)")),
        "the annotation names the module:\n{transpiled}"
    );
    let output = Command::new(&interpreter.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "program transpiled for {target} failed on {interpreter}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(interpreter, target);
}

/// from 3.10 nothing defers the annotations, so the class body would evaluate this one as the
/// class is made, were it not a string
#[test]
fn an_inferred_annotation_names_an_imported_module() {
    if let Some(interpreter) = interpreters::oldest(PythonVersion::PY310, "", "") {
        run_at(
            &interpreter,
            PythonVersion::PY310,
            true,
            PROGRAM,
            "\"decimal.Decimal\"",
        );
        run_at(
            &interpreter,
            PythonVersion::PY310,
            false,
            PROGRAM_EAGER,
            "\"decimal.Decimal\"",
        );
    }
}

/// below 3.10 the annotations are deferred, and `typing.get_type_hints` still evaluates them
#[test]
fn an_inferred_annotation_resolves_when_deferred() {
    if let Some(interpreter) = interpreters::oldest(
        PythonVersion::PY39,
        "import typing_extensions",
        "with `typing_extensions`",
    ) {
        run_at(
            &interpreter,
            PythonVersion::PY39,
            true,
            PROGRAM,
            "decimal.Decimal",
        );
    }
}

/// `x_mod` imports `m`, and so `b` imports it for a checker alone. `m`'s class holds a value
/// `b` declares an `x_mod.Thing`, and the annotation inferred for it names `x_mod`: evaluated as
/// the class is made, it would import `x_mod` while `m` is half made, and `x_mod`'s
/// `from m import C` would find no `C`
const CYCLE: &[(&str, &str)] = &[
    (
        "/main.by",
        "import m\n\nassert m.C.x.val() == 7\nprint(\"ok\")\n",
    ),
    ("/m.by", "from b import make\n\nclass C:\n    x = make()\n"),
    (
        "/b.by",
        r#"from typing import TYPE_CHECKING
if TYPE_CHECKING:
    import x_mod

class Impl:
    def val(self) -> int:
        return 7

def make() -> x_mod.Thing:
    return Impl()
"#,
    ),
    (
        "/x_mod.by",
        r#"from typing import Protocol
from m import C

class Thing(Protocol):
    def val(self) -> int: ...

def use(c: C): ...
"#,
    ),
];

/// the module an inferred annotation names is not imported into a cycle the source kept it
/// out of, on a python that evaluates the annotation as the class is made
#[test]
fn an_inferred_annotation_adds_no_import_cycle() {
    let Some(interpreter) = interpreters::oldest(PythonVersion::PY310, "", "") else {
        return;
    };
    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/"),
    ));
    db.init_program().expect("program init failed");
    for (path, source) in CYCLE {
        db.write_file(path, source).expect("write file failed");
    }
    for lazy_imports in [true, false] {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("inferred_annotation_cycle_{lazy_imports}"));
        // a stale directory from an earlier run would mask a transpile failure
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create case dir");
        let config = Config {
            min_version: PythonVersion::PY310,
            lazy_imports,
            ..Config::default()
        };
        for (path, _) in CYCLE {
            let file = system_path_to_file(&db, path).expect("file not in db");
            let transpiled = transpile_typed(&db, file, &config, None)
                .unwrap_or_else(|error| panic!("transpile of {path} should succeed: {error}"));
            let relative = path.trim_start_matches('/').replace(".by", ".py");
            fs::write(dir.join(relative), transpiled).expect("write module");
        }
        let m = fs::read_to_string(dir.join("m.py")).expect("read m");
        assert!(m.contains("x: \"x_mod.Thing\" = make()"), "{m}");
        let output = Command::new(&interpreter.command)
            .arg("main.py")
            .current_dir(&dir)
            .output()
            .expect("failed to spawn python");
        assert!(
            output.status.success(),
            "program failed on {interpreter}, lazy imports {lazy_imports}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- m ---\n{m}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
        interpreters::ran(&interpreter, PythonVersion::PY310);
    }
}
