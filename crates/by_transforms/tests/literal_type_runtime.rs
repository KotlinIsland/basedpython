//! Runtime test for a string in a `.by` type expression, which is a literal type wherever a
//! type expression holds it: the checker reads `list["a"]` as a list of the string `"a"`
//! (`basedpython_literal_annotations.md`, "a string in a generic's type argument is a
//! Literal"). The emitted python has to say the same to whatever reads it back, or
//! `typing.get_type_hints` reads the bare string as a forward reference to a name `a`
//!
//! A forward reference the transpiler quotes itself is a string python has to read as a
//! name, so it stays one, with the literal types inside it lowered

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod interpreters;

/// the shapes the mdtest reveals, read back through `typing`
const PROGRAM: &str = r#"
import typing
from typing import Callable, Literal, get_args, get_type_hints
from enum import Enum

class Colour(Enum):
    Red = 1

type Named = dict[str, "y"]
Alias = list["x"]

def f(a: list["a"], b: dict[str, "k"], c: Callable[["p"], "r"], n: Named, al: Alias) -> None:
    pass

def later(x: list["q"] | Later) -> None:
    pass

class Later:
    pass

hints = get_type_hints(f)
assert get_args(hints["a"]) == (Literal["a"],), hints["a"]
assert get_args(hints["b"]) == (str, Literal["k"]), hints["b"]
assert get_args(hints["c"]) == ([Literal["p"]], Literal["r"]), hints["c"]
assert get_args(Named.__value__) == (str, Literal["y"]), Named.__value__
assert get_args(Alias) == (Literal["x"],), Alias
quoted = get_type_hints(later)["x"]
assert get_args(quoted) == (list[Literal["q"]], Later), quoted
# a subscript that looks a member up by name holds no type at all
assert Colour["Red"] === Colour.Red
print("ok")
"#;

/// from 3.12, where `type` is a statement of its own
#[test]
fn a_string_type_argument_reads_back_as_its_literal_type() {
    let Some(interpreter) = interpreters::oldest(PythonVersion::PY312, "", "") else {
        return;
    };
    let config = Config {
        min_version: PythonVersion::PY312,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");
    let output = Command::new(&interpreter.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "failed on {interpreter}:\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(&interpreter, PythonVersion::PY312);
}
