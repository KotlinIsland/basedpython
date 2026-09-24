//! Runtime test for the builtins the lowerings write.
//!
//! A lowering that calls `isinstance` or decorates with `staticmethod` means the builtin, and
//! reads whatever the scope it lands in binds under that name. A module that binds the name
//! itself — a parameter named `isinstance`, a class attribute named `staticmethod` — has the
//! builtin read under a name of the lowering's own, imported from `builtins`. The transform
//! unit tests pin the spelling; this test runs it, below python 3.10, where `match` is
//! polyfilled too, and on the newest interpreter found.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod interpreters;
use interpreters::Interpreter;

const PROGRAM: &str = r#"
class A:
    staticmethod = 3

    static def m() -> int:
        return 1

def f(isinstance: int, x: object) -> bool:
    return x is int

def g(type: int, x: object) -> bool:
    return x is int | None

class E(Exception):
    pass

def r() -> int | E:
    return 3

def k(isinstance: int) -> int | E:
    y = r()^
    return y + isinstance

def n(len: int, xs: list[int]) -> int:
    match xs:
        case [a, b]:
            return a + b + len
    return 0

assert A.m() == 1 and A.staticmethod == 3
assert f(1, 2) and not f(1, "s")
assert g(1, None) and g(1, 2) and not g(1, "s")
assert k(1) == 4
assert n(10, [1, 2]) == 13 and n(10, [1]) == 0
print("ok")
"#;

/// a module that binds a builtin at its top level, read by a type test phase 0 lowers and by
/// the `match` polyfill and its runtime definitions after it — under one name, imported
/// ahead of the definitions
const MODULE_BINDING: &str = r#"
def isinstance(a: object, b: object) -> str:
    return "mine"

def f(x: object) -> str:
    if x is str:
        return "str"
    match x:
        case [a, b]:
            return "pair"
        case int():
            return "int"
    return "other"

assert f("s") == "str" and f([1, 2]) == "pair" and f(3) == "int" and f(None) == "other"
assert isinstance(1, int) == "mine"
print("ok")
"#;

/// a function that declares a builtin `global` and assigns it rebinds the module's, which the
/// definitions pasted into the module read through its globals
const GLOBAL_REBINDING: &str = r#"
def rebind():
    global isinstance
    isinstance = lambda a, b: True

def f(y: int?) -> int:
    return y!

rebind()
assert f(2) == 2
print("ok")
"#;

fn run_at(interpreter: &Interpreter, target: PythonVersion) {
    run_program_at(PROGRAM, interpreter, target);
    run_program_at(MODULE_BINDING, interpreter, target);
    run_program_at(GLOBAL_REBINDING, interpreter, target);
}

fn run_program_at(program: &str, interpreter: &Interpreter, target: PythonVersion) {
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");
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

#[test]
fn a_builtin_a_lowering_writes_is_the_builtin_below_python_310() {
    if let Some(interpreter) = interpreters::oldest(
        PythonVersion::PY39,
        "import typing_extensions",
        "with `typing_extensions`",
    ) {
        run_at(&interpreter, PythonVersion::PY39);
    }
}

#[test]
fn a_builtin_a_lowering_writes_is_the_builtin() {
    if let Some(interpreter) = interpreters::interpreters().pop() {
        run_at(&interpreter, interpreter.version);
    }
}
