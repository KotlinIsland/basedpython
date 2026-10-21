//! Runtime test for the classes nested synthesized types lower to.
//!
//! A typed dict literal, an anonymous named tuple and an inline protocol each lower to a class,
//! and each can be the type of another's field. The outer class's body has to name the inner
//! one's class: written as the source spells it, a typed dict field reads back from
//! `typing.get_type_hints` as a plain dict, `{'k': int}`, which is no type at all — with `by
//! check` clean and nothing failing until something reads the hints. A field naming a type
//! parameter has to read the one the generic declares, from module scope, where the class is.

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


assert typing.get_type_hints(typing.get_type_hints(C.get)["return"])["a"] === C.__type_params__[0]
print("ok")
"#;

#[test]
fn a_synthesized_type_inside_another_is_its_class() {
    let Some(python) = interpreters::oldest(
        PythonVersion::PY313,
        "import typing_extensions",
        "with `typing_extensions`",
    ) else {
        return;
    };
    let config = Config {
        min_version: PythonVersion::PY313,
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
    interpreters::ran(&python, PythonVersion::PY313);
}
