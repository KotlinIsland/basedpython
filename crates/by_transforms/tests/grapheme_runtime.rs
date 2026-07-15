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
//! can be found, the test skips rather than fails — it documents a runtime
//! dependency, it doesn't police the CI image.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

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

/// Locate an interpreter with `regex` importable: `$PYTHON` first, then a short
/// list of common names. Returns `None` (test skips) when none qualifies.
fn python_with_regex() -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("PYTHON") {
        candidates.push(p);
    }
    candidates.extend(["python3.13", "python3", "python"].map(String::from));

    candidates.into_iter().find(|py| {
        Command::new(py)
            .args(["-c", "import regex"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn grapheme_surface_runs_correctly() {
    let Some(python) = python_with_regex() else {
        eprintln!(
            "skipping grapheme runtime test: no interpreter with `regex` found \
             (set PYTHON to one with `pip install regex`)"
        );
        return;
    };

    let config = Config {
        min_version: PythonVersion::PY313,
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
        "transpiled grapheme program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
