//! Runtime divergence test for the grapheme string surface.
//!
//! The mdtest checker verifies the *types* of `character_count` / `first` /
//! `last` / `reversed` / `character_at` / `prefix` / `suffix` / `drop_first` /
//! `drop_last`;
//! the transform unit tests verify the *lowered text*. This test closes the
//! loop: it transpiles module-level assertions that actually *exercise* the
//! grapheme surface and runs them on a real interpreter, proving the emitted
//! `_by_graphemes` helpers compute grapheme-correct results (the US flag and a
//! ZWJ facepalm emoji are one `Character` each, not two / five code points).
//!
//! The grapheme helpers require the third-party `regex` package (the only
//! widely available UAX #29 engine). If no interpreter with `regex` importable
//! can be found, the test skips rather than fails, and says so — it documents a
//! runtime dependency, it doesn't police the CI image.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;

/// basedpython source whose module-level `assert`s exercise the grapheme
/// surface end to end. every string here is a single grapheme cluster made of
/// multiple code points, so a code-point implementation would give wrong
/// answers.
const PROGRAM: &str = r#"
from ty_extensions import Character

# US flag: two regional-indicator code points, one Character
assert "🇺🇸".character_count == 1, "flag count"
assert len("🇺🇸") == 2, "flag scalar length"

# facepalm: face + skin tone + ZWJ + male sign + variation selector = 5 scalars, 1 Character
assert "🤦🏼‍♂️".character_count == 1, "facepalm count"
assert len("🤦🏼‍♂️") == 5, "facepalm scalar length"

# a mixed string: 'a', flag, 'é' = three grapheme clusters
s = "a🇺🇸é"
assert s.character_count == 3, "mixed count"
assert s.first == "a", "first"
assert s.last == "é", "last"
assert s.character_at(1) == "🇺🇸", "character_at keeps the flag whole"
assert s.reversed == "é🇺🇸a", "reversed is grapheme-safe"
assert s.drop_first() == "🇺🇸é", "drop_first"
assert s.drop_last() == "a🇺🇸", "drop_last"
assert s.prefix(2) == "a🇺🇸", "prefix"
assert s.suffix(1) == "é", "suffix"
assert s.prefix(0) == "", "prefix(0)"
assert s.suffix(0) == "", "suffix(0)"
assert list(s.characters) == ["a", "🇺🇸", "é"], "characters"

# the scalar view still reaches the code points
assert len(list(s.unicode_scalars)) == 4, "unicode_scalars"

# python's occurrence-counting `count` method is left untouched
assert "mississippi".count("ss") == 2, "count"

# `Character` is a concrete class: the accessors construct real instances and
# it can be constructed explicitly
assert isinstance(s.first, Character), "first is a Character instance"
assert isinstance(s.character_at(1), Character), "character_at is a Character"
assert all(isinstance(c, Character) for c in s.characters), "characters"
made = Character("x")
assert isinstance(made, Character) and made == "x", "explicit construction"

# an annotated assignment materialises a real Character instance (the runtime
# value's class is Character, not a plain str)
annotated: Character = "a"
assert isinstance(annotated, Character), "annotation coerces to a Character"
assert type("a") is str, "a bare literal is still a plain str"

print("ok")
"#;

/// `first` and `last` bind a receiver that is more than a name to a temporary, which
/// python refuses to bind with `:=` in a comprehension's iterable or anywhere in a
/// comprehension in a class body
const BOUND_RECEIVER: &str = r#"
from ty_extensions import Character

def greeting() -> str:
    return "a🇺🇸é"

assert [c for c in [greeting().first, greeting().last]] == ["a", "é"], "in an iterable"

class Letters:
    let firsts: list[Character?] = [greeting().first for _ in [0]]

assert Letters.firsts == ["a"], "in a class body comprehension"

print("ok")
"#;

fn transpiled(program: &str, target: PythonVersion) -> String {
    let config = Config {
        min_version: target,
        ..Config::default()
    };
    transpile(program, &config).expect("transpile should succeed")
}

fn run_with_regex(program: &str) {
    let Some(interpreter) = common::python_with(
        PythonVersion::PY39,
        "import regex",
        "with `regex` (`pip install regex` into one, or set `PYTHON` to one that has it)",
    ) else {
        return;
    };
    let transpiled = transpiled(program, interpreter.version);

    let output = Command::new(&interpreter.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled grapheme program failed on {interpreter}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn grapheme_surface_runs_correctly() {
    run_with_regex(PROGRAM);
}

#[test]
fn a_bound_receiver_runs_where_python_refuses_an_assignment_expression() {
    run_with_regex(BOUND_RECEIVER);
}

/// the same module compiles, `regex` or not: an assignment expression where python
/// refuses one is a `SyntaxError` before the helpers that need `regex` ever run
#[test]
fn a_bound_receiver_compiles_where_python_refuses_an_assignment_expression() {
    let Some(interpreter) = common::python(PythonVersion::PY39) else {
        return;
    };
    let transpiled = transpiled(BOUND_RECEIVER, interpreter.version);

    let output = Command::new(&interpreter.command)
        .args([
            "-c",
            "import sys; compile(sys.argv[1], '<emitted>', 'exec')",
            &transpiled,
        ])
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "the transpiled grapheme program does not compile on {interpreter}:\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stderr),
    );
}
