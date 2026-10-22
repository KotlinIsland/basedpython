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
//! No third-party packages are needed, so the program is transpiled for whichever
//! interpreter runs it; if none is found the test skips, and says so, rather than
//! fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

use common::python;

mod common;
mod interpreters;

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
fn every_exported_name_exists_at_runtime() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };

    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");

    // `import *` needs a real module on the path, not a `-c` script
    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&python.command)
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
fn a_renamed_member_is_the_same_attribute_from_every_place_that_may_reach_it() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };

    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(MEMBERS, &config).expect("transpile should succeed");

    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&python.command)
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
fn a_renamed_variable_is_the_same_binding_wherever_it_is_the_symbol() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };

    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(VARIABLES, &config).expect("transpile should succeed");

    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&python.command)
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
fn a_renamed_name_is_read_back_wherever_python_looks_it_up() {
    let Some(python) = python(PythonVersion::PY39) else {
        return;
    };

    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(SHAPES, &config).expect("transpile should succeed");

    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&python.command)
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

/// module-level `private` symbols whose underscored name the module already has: a runtime
/// helper's (`_force_unwrap`, which `x!` calls), the author's own (`_shadowed`, `_theme`), and
/// ones a lowering takes for itself (the `_T` a polyfilled type parameter is declared under, the
/// `_MISSING` a mutable default is replaced by). each renamed symbol has to go somewhere else,
/// and every place that names it has to follow
const TAKEN_NAMES: &str = r#"
_shadowed = 7
_theme = "light"

private def force_unwrap(x: int) -> int:
    return x + 100

private def shadowed() -> int:
    return 1

private type T = int

def ident[T](x: T) -> T:
    return x

def holder(v: T):
    pass

def count(xs: list[int] = []) -> int:
    return len(xs)

private let MISSING: int = 5

private context let theme = "dark"

def paint(context theme: str) -> str:
    return theme

def first(x: int | None) -> int:
    return x!

def calls() -> tuple[int, int, int, int, int, str]:
    return (first(1), force_unwrap(1), shadowed(), count(), MISSING, paint())
"#;

const TAKEN_NAMES_IMPORTER: &str = r#"
import importlib, typing
m = importlib.import_module("emitted")
assert m.calls() == (1, 101, 1, 0, 5, "dark"), m.calls()
assert m._shadowed == 7 and m._theme == "light", "the author's own names keep their values"
assert typing.get_type_hints(m.holder)["v"].__value__ is int, "`T` is still the private alias"
assert m.ident(2) == 2
print("ok")
"#;

#[test]
fn a_renamed_symbol_never_takes_a_name_the_module_already_has() {
    let Some(interpreter) = interpreters::oldest(
        PythonVersion::PY39,
        "import typing_extensions",
        "with `typing_extensions`",
    ) else {
        return;
    };
    // the oldest target the interpreter runs, where the type parameter is polyfilled
    let target = interpreter.version.min(PythonVersion::PY311);
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    let transpiled = transpile(TAKEN_NAMES, &config).expect("transpile should succeed");

    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&interpreter.command)
        .arg("-c")
        .arg(TAKEN_NAMES_IMPORTER)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "importing the transpiled module failed on {interpreter}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(&interpreter, target);
}

/// a module-level `private` class named by the checks a lowering writes from a type rather
/// than from the source: the soundness checks on a loop, an assignment, a return, an
/// argument and a projection — through a container, through a private type alias, from a
/// method of another class, and against a private generic's specialization, by a public
/// argument and by a private one
const SOUNDNESS_CHECKS: &str = r#"
from typing import Any

private class Kept:
    init(let n: int)

private class Box[T]:
    init(let v: T)

private type Alias = Kept

private var kept: list[Kept] = [Kept(1), Kept(2)]

def dynamic(value: object) -> Any:
    return value

def looped() -> int:
    var sum = 0
    for one in kept:
        sum += one.n
    return sum

def assigned() -> int:
    k: Kept = dynamic(Kept(3))
    return k.n

def returned() -> Kept:
    return dynamic(Kept(4))

def takes(k: Kept) -> int:
    return k.n

def argument() -> int:
    return takes(dynamic(Kept(5)))

def projected() -> int:
    nested: dict[str, list[Kept]] = {"a": [Kept(6)]}
    return nested["a"][0].n

def aliased() -> int:
    a: Alias = dynamic(Kept(7))
    return a.n

class Reader:
    def read(self) -> int:
        var sum = 0
        for one in kept:
            sum += one.n
        return sum

def boxed() -> int:
    boxes: list[Box[int]] = [Box[int](8)]
    var sum = 0
    for box in boxes:
        sum += box.v
    return sum

def kept_boxes() -> int:
    boxes: list[Box[Kept]] = [Box[Kept](Kept(9))]
    var sum = 0
    for box in boxes:
        sum += box.v.n
    return sum

def results() -> list[int]:
    return [looped(), assigned(), returned().n, argument(), projected(), aliased(), Reader().read(), boxed(), kept_boxes()]
"#;

const SOUNDNESS_CHECKS_IMPORTER: &str = r#"
import importlib
m = importlib.import_module("emitted")
assert m.results() == [3, 3, 4, 5, 6, 7, 3, 8, 9], m.results()
m.dynamic = lambda value: "not kept"
try:
    m.returned()
except TypeError as error:
    assert "_Kept" in str(error), error
else:
    raise AssertionError("the return check let a `str` through")
assert not hasattr(m, "Kept") and not hasattr(m, "Box")
print("ok")
"#;

#[test]
fn a_soundness_check_names_a_private_class_by_its_renamed_binding() {
    // the program declares a generic class with pep 695 syntax
    let Some(interpreter) = interpreters::oldest(PythonVersion::PY312, "", "") else {
        return;
    };
    let target = interpreter.version.min(PythonVersion::PY313);
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    let transpiled = transpile(SOUNDNESS_CHECKS, &config).expect("transpile should succeed");

    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&interpreter.command)
        .arg("-c")
        .arg(SOUNDNESS_CHECKS_IMPORTER)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "importing the transpiled module failed on {interpreter}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(&interpreter, target);
}

/// a module-level `private` class named by the other lowerings that write a type out at
/// runtime: an `is` type test against a class, a generic, a protocol's member and an enum
/// member, the checked casts, the specialization a reified generic and an erased union are
/// called with, the guard a `raises` clause is checked by, and the annotation inferred for
/// a class attribute, which python evaluates as the class body runs
const RUNTIME_TYPES: &str = r#"
from enum import Enum
from typing import Protocol

private class Kept:
    init(let n: int)

private class Box[T]:
    init(let v: T)

private class Oops(Exception):
    pass

class Holder[T]:
    init(let v: T)

    def kind(self) -> object:
        return T

def kind_of[T](value: T) -> object:
    return T

def is_kept(value: object) -> bool:
    return value is Kept

def is_int_box(value: object) -> bool:
    return value is Box[int]

private class HasKept(Protocol):
    k: Kept

    def get(self, other: Kept) -> Kept: ...

class Shaped:
    k: Kept = Kept(6)

    def get(self, other: Kept) -> Kept:
        return other

def has_kept(value: object) -> bool:
    return value is HasKept

private class Color(Enum):
    RED = 1
    BLUE = 2

def is_red(value: object) -> bool:
    return value is Color.RED

def count(values: list[Kept] | list[int]) -> int:
    return len(values)

def counted() -> int:
    return count([Kept(7), Kept(8)])

def checked(value: object) -> int:
    return (value cast! Kept).n

def tried(value: object) -> Kept?:
    return value cast? Kept

def held() -> object:
    return Holder(Kept(1)).kind()

def called() -> object:
    return kind_of(Kept(2))

def fails() -> int raises Oops:
    raise Oops()

def rethrow[reified E: Exception](error: E) raises E | Oops:
    raise error

def made() -> Kept:
    return Kept(5)

class Shelf:
    var item = made()
"#;

const RUNTIME_TYPES_IMPORTER: &str = r#"
import importlib
m = importlib.import_module("emitted")
kept = m._Kept(3)
assert m.is_kept(kept) and not m.is_kept(3)
assert m.is_int_box(m._Box[int](4)) and not m.is_int_box(m._Box[str]("4"))
assert m.has_kept(m.Shaped()) and not m.has_kept(object())
assert m.is_red(m._Color.RED) and not m.is_red(m._Color.BLUE)
assert m.counted() == 2
assert m.checked(kept) == 3
try:
    m.checked(3)
except TypeError:
    pass
else:
    raise AssertionError("`cast!` let an `int` through")
assert m.tried(kept) is kept and m.tried(3) is None
assert m.held() is m._Kept and m.called() is m._Kept
try:
    m.fails()
except m._Oops:
    pass
# the guard reads `E` off the specialization, and tests `(E, _Oops)` rather than the
# ceiling `Exception`, which would let the `KeyError` through
for raised in (ValueError("v"), m._Oops()):
    try:
        m.rethrow[ValueError](raised)
    except type(raised):
        pass
try:
    m.rethrow[ValueError](KeyError("k"))
except AssertionError as error:
    assert "rethrow raised KeyError" in str(error), error
else:
    raise AssertionError("the `raises` guard let a `KeyError` through")
assert m.Shelf.__annotations__["item"] is m._Kept
print("ok")
"#;

#[test]
fn a_type_written_out_at_runtime_names_a_private_class_by_its_renamed_binding() {
    // the program declares a generic class with pep 695 syntax
    let Some(interpreter) = interpreters::oldest(PythonVersion::PY312, "", "") else {
        return;
    };
    let target = interpreter.version.min(PythonVersion::PY313);
    let config = Config {
        min_version: target,
        runtime_raises_checks: true,
        ..Config::default()
    };
    let transpiled = transpile(RUNTIME_TYPES, &config).expect("transpile should succeed");

    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    std::fs::write(dir.path().join("emitted.py"), &transpiled).expect("failed to write the module");

    let output = Command::new(&interpreter.command)
        .arg("-c")
        .arg(RUNTIME_TYPES_IMPORTER)
        .current_dir(dir.path())
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "importing the transpiled module failed on {interpreter}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(&interpreter, target);
}
