//! Runtime test for forward references in annotations python evaluates as the definition runs.
//!
//! basedpython resolves every annotation as deferred, so a signature may name a class defined
//! further down, the class it sits in, or a name imported only for the checker. Before 3.14
//! python evaluates those annotations as the `def` or the class body runs, and an unquoted
//! forward reference raises `NameError` at import — which asserting on the lowered text cannot
//! see.
//!
//! Needs an interpreter older than 3.14. From 3.14 annotations are deferred natively, so there
//! is nothing to observe, and the test skips rather than pass without checking anything.
//!
//! Below 3.10 the output defers every annotation with `from __future__ import annotations`
//! instead, and there a quote would reach `__annotations__` as a string inside a string, which
//! a library reading the annotation as text takes for something else.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

mod interpreters;

/// every place an annotation runs with the definition, naming something bound after it
const PROGRAM: &str = r#"
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections import OrderedDict


def make() -> Later:
    return Later()


class Node:
    parent: Node?

    def child(self, other: Node) -> list[Node]:
        return [other]

    def later(self) -> Later?:
        return None


value: Later


def ordered(counts: OrderedDict[str, int]) -> None: ...


class Later: ...


assert type(make()) == Later, "a function above the class it returns runs"
assert len(Node().child(Node())) == 1, "so does a method naming its own class"
print("ok")
"#;

/// whether `python` still evaluates annotations as the definition runs
fn evaluates_annotations_eagerly(python: &str) -> bool {
    Command::new(python)
        .args([
            "-c",
            "import sys; raise SystemExit(sys.version_info >= (3, 14))",
        ])
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_forward_reference_does_not_raise_at_import() {
    let Some(python) = python() else {
        return;
    };
    if !evaluates_annotations_eagerly(&python) {
        eprintln!("skipping: {python} defers annotations natively, so nothing is evaluated early");
        return;
    }
    let config = Config {
        min_version: PythonVersion::PY310,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");
    let output = Command::new(&python)
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

/// a forward reference in a module transpiled for 3.9, which defers every annotation with
/// `from __future__ import annotations`. quoted as well, `ClassVar[Later]` reached
/// `__annotations__` as `"'ClassVar[Later]'"`, which `dataclasses` does not read as a class
/// variable, so it became a field the constructor required
const DEFERRED_BY_THE_FUTURE_IMPORT: &str = r#"
from dataclasses import dataclass, fields
from typing import ClassVar, TypedDict, get_type_hints


def make(x: Later) -> Later:
    return x


class Holder:
    item: Later


@dataclass
class Record:
    shared: ClassVar[Later]
    item: Later


class Shape(TypedDict):
    item: Later


class Later: ...


assert make.__annotations__ == {"x": "Later", "return": "Later"}, make.__annotations__
assert Holder.__annotations__ == {"item": "Later"}, Holder.__annotations__
assert [field.name for field in fields(Record)] == ["item"], fields(Record)
for target, name in ((make, "x"), (Holder, "item"), (Record, "item"), (Shape, "item")):
    assert get_type_hints(target)[name] == Later, get_type_hints(target)
print("ok")
"#;

#[test]
fn a_forward_reference_under_the_future_import_is_not_quoted() {
    let target = PythonVersion::PY39;
    let Some(interpreter) = interpreters::oldest(target, "", "") else {
        return;
    };
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    let transpiled =
        transpile(DEFERRED_BY_THE_FUTURE_IMPORT, &config).expect("transpile should succeed");
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
    interpreters::ran(&interpreter, target);
}
