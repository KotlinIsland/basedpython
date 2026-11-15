//! runtime test for the class a typed dict literal lowers to
//!
//! the class says whether it is closed with `closed=` or `extra_items=`, which `typing.TypedDict`
//! takes only from 3.15. declared on `typing_extensions.TypedDict` instead, a module for an older
//! python checked clean and raised `ModuleNotFoundError` the moment it was imported anywhere
//! `typing_extensions` was not installed, which nothing that runs the module installs. so the
//! program runs here with `typing_extensions` unimportable, and reads back what a reader of the
//! class can observe: a `TypedDict` by `typing`'s own test, plain dicts for instances, every key
//! required and none read-only, and what 3.15 records of whether it is closed

mod common;
mod interpreters;

use std::io::Write;
use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

use crate::interpreters::Interpreter;

const PROGRAM: &str = r#"
import typing
from typing import cast

absent = object()

def f(a: {"name": str, "at": {"x": int, "y": int}}, b: {"k": int, **: str}) -> None: ...

hints = typing.get_type_hints(f)
a = hints["a"]
b = hints["b"]
at = typing.get_type_hints(a)["at"]

assert typing.get_type_hints(a) == {"name": str, "at": at}, typing.get_type_hints(a)
assert typing.get_type_hints(at) == {"x": int, "y": int}, typing.get_type_hints(at)
assert typing.get_type_hints(b) == {"k": int}, typing.get_type_hints(b)
if hasattr(typing, "is_typeddict"):
    assert typing.is_typeddict(a) and typing.is_typeddict(at) and typing.is_typeddict(b)

assert a.__required_keys__ == frozenset({"name", "at"}), a.__required_keys__
assert a.__optional_keys__ == frozenset(), a.__optional_keys__
assert a.__total__
assert a.__readonly_keys__ == frozenset(), a.__readonly_keys__
assert a.__mutable_keys__ == frozenset({"name", "at"}), a.__mutable_keys__

assert a.__closed__ === True and at.__closed__ === True, (a.__closed__, at.__closed__)
assert getattr(a, "__extra_items__", absent) === getattr(typing, "NoExtraItems", absent)
assert b.__closed__ === None, b.__closed__
assert b.__extra_items__ == "str", b.__extra_items__

value = a(name="n", at={"x": 1, "y": 2})
assert type(value) === dict and value == {"name": "n", "at": {"x": 1, "y": 2}}, value
assert cast({"name": str, "at": {"x": int, "y": int}}, value) === value
try:
    isinstance(value, a)
except TypeError:
    pass
else:
    raise AssertionError("a TypedDict cannot be tested against")
print("ok")
"#;

/// the program transpiled for `target`, run on `python` with `typing_extensions` unimportable,
/// and what it was transpiled to
fn run_at(python: &Interpreter, target: PythonVersion) -> String {
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");
    let mut file = tempfile::NamedTempFile::new().expect("failed to create the source file");
    file.write_all(transpiled.as_bytes())
        .expect("failed to write the source file");
    // `None` in `sys.modules` makes every import of the name raise `ModuleNotFoundError`, as it
    // does where the package is not installed, whatever this interpreter has installed
    let output = Command::new(&python.command)
        .args([
            "-c",
            "import sys; sys.modules['typing_extensions'] = None; \
             exec(compile(open(sys.argv[1], encoding='utf-8').read(), '<string>', 'exec'))",
        ])
        .arg(file.path())
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "failed on {python}:\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    transpiled
}

/// below 3.15 the runtime makes the class, on every interpreter there is, each running what is
/// transpiled for it
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn below_315_the_runtime_makes_the_class() {
    let found = interpreters::interpreters();
    let older: Vec<&Interpreter> = found
        .iter()
        .filter(|python| (PythonVersion::PY39..PythonVersion::PY315).contains(&python.version))
        .collect();
    if older.is_empty() {
        eprintln!("skipping: no interpreter of python 3.9 to 3.14 found");
    }
    for python in older {
        interpreters::ran(python, python.version);
        let transpiled = run_at(python, python.version);
        assert!(transpiled.contains("_ByTypedDict"), "{transpiled}");
    }
}

/// from 3.15 `typing.TypedDict` makes the class itself, imported lazily as every import is there
#[test]
fn from_315_typing_makes_the_class() {
    let Some(python) = common::python(PythonVersion::PY315) else {
        return;
    };
    let transpiled = run_at(&python, python.version);
    assert!(
        transpiled.contains("lazy from typing import TypedDict\n")
            && !transpiled.contains("_ByTypedDict"),
        "{transpiled}"
    );
}
