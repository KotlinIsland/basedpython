//! Runtime test for the `__all__` the visibility modifiers synthesize.
//!
//! `__all__` is the one visibility artefact with teeth at runtime: `from m
//! import *` reads it and looks every entry up, so a name in it that the module
//! does not have raises `AttributeError` at import. That makes it the place
//! where a disagreement between the list and the emitted names is fatal rather
//! than cosmetic — `private` renames its symbol, so a symbol marked both
//! `export` and `private` listed a name nothing answers to.
//!
//! The transform unit tests pin the text of the emitted `__all__`. This test
//! closes the loop by importing the module the way `__all__` is for.
//!
//! No third-party packages are needed, so any `python3` will do; if none is
//! found the test skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

/// Every visibility shape that reaches `__all__`, in both keyword orders.
const PROGRAM: &str = r#"
export private def helper() -> int:
    return 1

private export class Hidden:
    pass

export def api() -> int:
    return helper()

public class Shown:
    pass

def unmarked() -> int:
    return 2
"#;

/// Import the transpiled module the way `import *` does, and report what the
/// module actually offers so a mismatch names both sides.
const IMPORTER: &str = r#"
import importlib, sys
m = importlib.import_module("emitted")
missing = [n for n in m.__all__ if not hasattr(m, n)]
assert not missing, f"__all__ names {missing}, which the module does not have"
from emitted import *
assert sorted(m.__all__) == ["Shown", "api"], f"unexpected __all__: {m.__all__}"
assert not hasattr(m, "helper") and hasattr(m, "_helper"), "`private` still renames"
assert not hasattr(m, "Hidden") and hasattr(m, "_Hidden"), "in either keyword order"
assert hasattr(m, "unmarked"), "an unmarked symbol is emitted, just not exported"
print("ok")
"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn every_exported_name_exists_at_runtime() {
    let Some(python) = python() else {
        eprintln!("skipping visibility runtime test: no `python3` interpreter found");
        return;
    };

    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");

    // `import *` needs a real module on the path, not a `-c` script
    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&python)
        .arg("-c")
        .arg(IMPORTER)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "importing the transpiled module failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

/// Every class-member shape a visibility keyword renames, read back from the
/// places allowed to reach it.
const MEMBERS: &str = r#"
class Counter:
    protected step: int = 2
    private total: int = 0

    init(private let label: str, protected let limit: int)

    private def bump(self) -> int:
        self.total = self.total + self.step
        return self.total

    def run(self) -> str:
        while self.total < self.limit:
            self.bump()
        return f"{self.label}:{self.total}"


class Loud(Counter):
    def describe(self) -> str:
        return f"{self.step}/{self.limit}"
"#;

/// The emitted names are what python actually stores, so the check is made
/// against `vars()` as well as against the values: a rename that lands on the
/// wrong spelling still reads back correctly from inside the class, and only
/// the stored name gives it away.
const MEMBER_IMPORTER: &str = r#"
import importlib
m = importlib.import_module("emitted")
c = m.Counter("a", 5)
assert c.run() == "a:6", c.run()
assert m.Loud("b", 3).describe() == "2/3"

stored = vars(c)
assert "_Counter__label" in stored, stored
assert "_limit" in stored, stored
assert "_Counter__total" in stored, stored
assert not hasattr(c, "label") and not hasattr(c, "total"), "a private member is mangled"
assert c._step == 2 and c._Counter__total == 6
assert not hasattr(c, "bump") and callable(c._Counter__bump)
print("ok")
"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_renamed_member_is_the_same_attribute_from_every_place_that_may_reach_it() {
    let Some(python) = python() else {
        eprintln!("skipping member visibility runtime test: no `python3` interpreter found");
        return;
    };

    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(MEMBERS, &config).expect("transpile should succeed");

    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&python)
        .arg("-c")
        .arg(MEMBER_IMPORTER)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "importing the transpiled module failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

/// A module-level `private` variable and a `private` class variable, reached
/// from every place that may reach each — and a parameter and a class attribute
/// that share the module variable's name, which are other bindings entirely.
const VARIABLES: &str = r#"
private count: int = 0

def bump() -> int:
    global count
    count += 1
    return count

def shadow(count: int) -> int:
    return count * 10

class Holder:
    count = 5

    def module_count(self) -> int:
        return count

class Registry:
    private class var made: int = 0

    init():
        Registry.made += 1

    @classmethod
    def total(cls) -> int:
        return cls.made
"#;

const VARIABLE_IMPORTER: &str = r#"
import importlib
m = importlib.import_module("emitted")
assert m.bump() == 1 and m.bump() == 2
assert m.shadow(3) == 30, "a parameter that shares the name is its own binding"
assert m.Holder.count == 5, "so is a class attribute"
assert m.Holder().module_count() == 2, "a method reads past its class body to the module"
assert hasattr(m, "_count") and not hasattr(m, "count"), "the module variable is renamed"
m.Registry()
m.Registry()
assert m.Registry.total() == 2
assert "_Registry__made" in vars(m.Registry) and not hasattr(m.Registry, "made")
print("ok")
"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_renamed_variable_is_the_same_binding_wherever_it_is_the_symbol() {
    let Some(python) = python() else {
        eprintln!("skipping variable visibility runtime test: no `python3` interpreter found");
        return;
    };

    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(VARIABLES, &config).expect("transpile should succeed");

    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&python)
        .arg("-c")
        .arg(VARIABLE_IMPORTER)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "importing the transpiled module failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

/// Every place a class body can declare a member, every list of member names,
/// and every statement that rebinds a module-level `private` name — each read
/// back the way python reads it.
const SHAPES: &str = r#"
private last: int = 0
private err: Exception | None = None
private def loads(s: str) -> int:
    return 0
from json import loads

try:
    raise ValueError("boom")
except ValueError as err:
    pass

match 7:
    case last:
        pass

class Shape:
    if True:
        private sides: int = 3
    private scale: int = 2
    doubled = scale * 2

    private class Inner:
        pass

    @staticmethod
    private def make() -> int:
        return 4

    def area(self) -> int:
        return self.sides * self.scale + self.make()

    def inner(self) -> object:
        return Shape.Inner()

class Slotted:
    __slots__ = ("x",)
    private x: int

    def __init__(self, x: int):
        self.x = x

    def get(self) -> int:
        return self.x

class Point:
    __match_args__ = ("x",)
    private x: int

    def __init__(self, x: int):
        self.x = x

    def unpack(self) -> int:
        match self:
            case Point(x=v):
                return v
        return -1

class Base:
    protected def hook(self) -> int:
        return 1

class Child(Base):
    protected override def hook(self) -> int:
        return super().hook() + 1

    def pick(self, o: Child | Other) -> int:
        return o.hook()

class Other(Base): ...
"#;

const SHAPE_IMPORTER: &str = r#"
import importlib
m = importlib.import_module("emitted")
s = m.Shape()
assert s.area() == 10 and m.Shape.doubled == 4
assert type(s.inner()).__name__ == "__Inner"
assert m._last == 7, "a match capture binds the renamed variable"
assert m._loads("[1]") == [1], "so does an import"
assert not hasattr(m, "loads") and not hasattr(m, "last") and not hasattr(m, "err")
slotted = m.Slotted(5)
assert slotted.get() == 5 and not hasattr(slotted, "__dict__"), "the slot is the member"
assert m.Point(9).unpack() == 9, "a class pattern's keyword reads `__match_args__`'s name"
c = m.Child()
assert c.pick(c) == 2 and c.pick(m.Other()) == 1
print("ok")
"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_renamed_name_is_read_back_wherever_python_looks_it_up() {
    let Some(python) = python() else {
        eprintln!("skipping visibility shapes runtime test: no `python3` interpreter found");
        return;
    };

    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(SHAPES, &config).expect("transpile should succeed");

    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&python)
        .arg("-c")
        .arg(SHAPE_IMPORTER)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "importing the transpiled module failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
