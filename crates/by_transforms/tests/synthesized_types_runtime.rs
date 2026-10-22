//! Runtime test for the classes nested synthesized types lower to.
//!
//! A typed dict literal, an anonymous named tuple and an inline protocol each lower to a class,
//! and each can be the type of another's field. The outer class's body has to name the inner
//! one's class: written as the source spells it, a typed dict field reads back from
//! `typing.get_type_hints` as a plain dict, `{'k': int}`, which is no type at all — with `by
//! check` clean and nothing failing until something reads the hints. A field naming a type
//! parameter has to read the one the generic declares, from module scope, where the class is.

mod common;
mod interpreters;

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

const PROGRAM: &str = r#"
import typing
import typing_extensions

def f(
    a: (p: {"k": int}, q: int),
    b: protocol(p: {"k": int}),
    c: {"k": (p: int, q: str)},
    d: {"k": protocol(n: int)},
    e: (p: protocol(n: int), q: int),
    g: protocol(p: (x: int, y: str)),
) -> None: ...

hints = typing.get_type_hints(f)

def field(name: str, member: str) -> object:
    return typing.get_type_hints(hints[name])[member]

assert typing_extensions.is_typeddict(field("a", "p")), field("a", "p")
assert typing.get_type_hints(field("a", "p")) == {"k": int}, "a"
assert typing_extensions.is_typeddict(field("b", "p")), field("b", "p")
assert field("c", "k")._fields == ("p", "q"), field("c", "k")
assert typing.get_type_hints(field("c", "k")) == {"p": int, "q": str}, "c"
assert typing.get_type_hints(field("d", "k")) == {"n": int}, field("d", "k")
assert typing.get_type_hints(field("e", "p")) == {"n": int}, field("e", "p")
assert field("g", "p")._fields == ("x", "y"), field("g", "p")


class C[T]:
    def get(self) -> {"a": T}: ...


assert typing.get_type_hints(typing.get_type_hints(C.get)["return"])["a"] === C.__parameters__[0]
print("ok")
"#;

#[test]
fn a_synthesized_type_inside_another_is_its_class() {
    let Some(python) = common::python_with(
        PythonVersion::PY39,
        "import typing_extensions",
        "with `typing_extensions`",
    ) else {
        return;
    };
    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");
    let output = Command::new(&python.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "failed on {python}:\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

/// A named tuple class is declared ahead of everything the module binds, and below 3.14 python
/// evaluates its field annotations as it makes the class. A field naming a class or an import of
/// the module, declared above or below the type, is not bound there yet, and raised `NameError`
/// on import of a module that checks clean. Each reads back, once the module has run, as the
/// class it names.
const MODULE_NAMES: &str = r#"
import typing
from fractions import Fraction

class Earlier: ...

def f(p: (a: Earlier, b: Later, c: Fraction, d: int)) -> None: ...

v = (e=Earlier(), g=Fraction(1), n=1)

class Later: ...

hints = typing.get_type_hints(typing.get_type_hints(f)["p"])
assert hints == {"a": Earlier, "b": Later, "c": Fraction, "d": int}, hints
value_hints = typing.get_type_hints(type(v))
assert value_hints == {"e": Earlier, "g": Fraction, "n": int}, value_hints
print("ok")
"#;

#[test]
fn a_field_naming_a_binding_of_the_module_reads_it() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled = transpile(MODULE_NAMES, &config).expect("transpile should succeed");
    let output = Command::new(&python.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "failed on {python}:\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
