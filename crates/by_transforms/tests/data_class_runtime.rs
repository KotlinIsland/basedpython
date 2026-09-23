//! Runtime test for the slots a `data class` is given.
//!
//! A `data class` is a slotted dataclass. `dataclass(slots=True)` is new in python 3.10, so
//! below it the runtime's `_by_dataclass_slots` does what that option does. The transform
//! unit tests pin which of the two a target gets; this test runs both and checks they
//! behave alike.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod interpreters;
use interpreters::Interpreter;

const PROGRAM: &str = r#"
import pickle

data class Pair:
    a: int
    b: list[int] = []

    def total(self) -> int:
        return self.a + len(self.b)

frozen data class Point:
    x: int
    y: int = 0

data class Child(Pair):
    c: int = 4

    override def total(self) -> int:
        return super().total() + self.c

p = Pair(1)
assert getattr(Pair, "__slots__") == ("a", "b") and not hasattr(p, "__dict__")
try:
    setattr(p, "extra", 1)
except AttributeError:
    pass
else:
    raise AssertionError("a slotted instance takes no attribute it does not declare")
p.b.append(2)
assert Pair(1).b == [], "a mutable default is still made per instance"
q = Point(1)
assert pickle.loads(pickle.dumps(q)) == q, "a frozen slotted instance round-trips"
assert getattr(Child, "__slots__") == ("c",), "a base's slots are not declared again"
assert Child(1, [1, 2]).total() == 7, "a zero-argument `super()` reaches the slotted class"
print("ok")
"#;

fn run_at(interpreter: &Interpreter, target: PythonVersion) {
    let config = Config {
        min_version: target,
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
        "program transpiled for {target} failed on {interpreter}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(interpreter, target);
}

#[test]
fn a_data_class_is_slotted_below_python_310() {
    if let Some(interpreter) = interpreters::oldest(
        PythonVersion::PY39,
        "import typing_extensions",
        "with `typing_extensions`",
    ) {
        run_at(&interpreter, PythonVersion::PY39);
    }
}

/// `dataclass(slots=True)` itself. the `override` the program writes is `typing`'s from 3.12,
/// and python's own `slots=True` points a zero-argument `super()` at the class it makes from
/// 3.13 on
#[test]
fn a_data_class_is_slotted_from_python_310() {
    if let Some(interpreter) = interpreters::oldest(PythonVersion::PY313, "", "") {
        run_at(&interpreter, PythonVersion::PY312);
    }
}
