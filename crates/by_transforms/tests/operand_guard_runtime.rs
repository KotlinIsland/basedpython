//! Runtime test for the test a `.by` operator dunder makes of its operand.
//!
//! The checker reads an operand outside a dunder's parameter domain as one the dunder hands
//! back, so that python moves on to the other operand. The transform's unit tests assert on
//! the lowered *text*; this runs the program, so the dispatch python actually makes is the one
//! asserted: the reflected dunder answers, `+=` moves on to `__add__`, a comparison falls back
//! to identity, and an operand inside the domain reaches the body.
//!
//! No third-party packages are needed, so the program is transpiled for whichever interpreter
//! runs it; if none is found the test skips, and says so, rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;
use common::python;
use interpreters::Interpreter;

const DISPATCH: &str = r#"
from typing import SupportsIndex

class A:
    def __add__(self, other: int) -> str:
        return "A.__add__"

    def __iadd__(self, other: int) -> str:
        return "A.__iadd__"

    def __sub__(self, other: B | None) -> str:
        return "A.__sub__"

    def __mul__(self, other: SupportsIndex) -> int:
        return other.__index__()

    def __eq__(self, other: A) -> bool:
        return True

class B:
    def __radd__(self, other: A) -> str:
        return "B.__radd__"

    def __rsub__(self, other: object) -> str:
        return "B.__rsub__"

    def __rmul__(self, other: object) -> str:
        return "B.__rmul__"

class Text:
    def __iadd__(self, other: int) -> str:
        return "Text.__iadd__"

    def __add__(self, other: str) -> str:
        return "Text.__add__"

# an operand inside the domain reaches the body
assert A() + 1 == "A.__add__"
# one outside it is handed back, and python asks the other operand
assert A() + B() == "B.__radd__"
# a class declared further down is read when the method is called
assert A() - B() == "A.__sub__"
assert A() - None == "A.__sub__"
# a runtime-checkable protocol is tested by its members
assert A() * 3 == 3
assert A() * B() == "B.__rmul__"

# an in-place dunder that hands its operand back moves on to the binary one
text = Text()
text += "a"
assert text == "Text.__add__", text
text = Text()
text += 1
assert text == "Text.__iadd__", text

# a comparison that hands its operand back falls back to identity
a = A()
assert a == A()
assert not (a == 1)
assert a != 1

print("ok")
"#;

fn run(python: &Interpreter, program: &str) {
    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");
    let output = Command::new(&python.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn an_operand_outside_the_domain_is_handed_back() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };
    run(&python, DISPATCH);
}
