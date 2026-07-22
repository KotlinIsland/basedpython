//! Runtime divergence test for parametric protocol checks — both `is`-tests
//! (`value is A[int]`) and checked casts (`value cast A[int]`, `value cast?
//! A[int]`).
//!
//! The unit tests verify the *lowered text* and the mdtest checker verifies the
//! *types*; this test closes the loop by running the structural protocol check
//! on a real interpreter. A protocol target carries no `__orig_class__`, so the
//! runtime residue reads the value's *reified class annotations* — the whole
//! point of the feature — and a broken annotation walk, a wrong variance
//! direction, or a `get_type_hints` failure would fail here even though the
//! text-level tests pass. The cast cases additionally confirm that a
//! method-bearing protocol degrades to an unchecked pass-through rather than
//! raising a `TypeError` from an `isinstance` against the protocol.
//!
//! PEP 695 class syntax needs a 3.13 interpreter; if none is found the test
//! skips rather than fails.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

/// basedpython whose module-level `assert`s exercise the structural protocol
/// check end to end: an exact match, a mismatch, an inferred class-body
/// annotation, a covariant read-only member, a nested-generic member, a missing
/// member, and an inheriting subclass.
const PROGRAM: &str = r#"
from typing import Protocol

class HasA[T](Protocol):
    a: T

# the headline case: `a = True` gets a reified `bool` annotation, so it matches
# `HasA[bool]` exactly but not `HasA[int]`
class C:
    a = True

assert (C() is HasA[bool]) is True, "reified bool annotation matches HasA[bool]"
assert (C() is HasA[int]) is False, "reified bool annotation does not match HasA[int]"

# an explicit annotation is checked the same way
class D:
    a: int

assert (D() is HasA[int]) is True, "explicit int annotation matches HasA[int]"
assert (D() is HasA[str]) is False, "explicit int annotation does not match HasA[str]"

# a missing member never matches
class Empty:
    pass

assert (Empty() is HasA[int]) is False, "missing member does not match"

# an annotation inherited from a base is found via the mro walk
class Base:
    a: int

class Sub(Base):
    pass

assert (Sub() is HasA[int]) is True, "inherited annotation matches"

# a read-only property member is covariant: the value's annotation need only be
# a subtype of the target argument
class HasRO[T](Protocol):
    @property
    def a(self) -> T: ...

class BoolAttr:
    a: bool

assert (BoolAttr() is HasRO[int]) is True, "bool annotation is a subtype of int (covariant)"
assert (BoolAttr() is HasRO[bool]) is True, "bool annotation matches bool exactly (covariant)"

class IntAttr:
    a: int

assert (IntAttr() is HasRO[bool]) is False, "int annotation is not a subtype of bool (covariant)"

# a nested-generic member spells the specialized type; a matching annotation
# passes, a differing argument fails
class HasList[T](Protocol):
    a: list[T]

class ListInt:
    a: list[int]

assert (ListInt() is HasList[int]) is True, "list[int] annotation matches HasList[int]"
assert (ListInt() is HasList[str]) is False, "list[int] annotation does not match HasList[str]"

# a multi-member protocol requires every member to match
class Pair[K, V](Protocol):
    a: K
    b: V

class Both:
    a: int
    b: str

class OnlyA:
    a: int

assert (Both() is Pair[int, str]) is True, "both members match"
assert (Both() is Pair[int, int]) is False, "second member mismatches"
assert (OnlyA() is Pair[int, str]) is False, "missing second member fails"

# `is not` negates the whole check
assert (C() is not HasA[int]) is True, "is not negates a non-match"
assert (C() is not HasA[bool]) is False, "is not negates a match"

# a checked `cast` to a data-member protocol validates the same structural
# claim: it returns the value on a match and raises on a mismatch
assert (C() cast HasA[bool]) is not None, "checked cast to a matching protocol returns the value"

_raised = False
try:
    D() cast HasA[str]
except TypeError:
    _raised = True
assert _raised, "checked cast to a non-matching protocol raises"

# `cast?` yields the value on a match and `None` on a mismatch
assert (D() cast? HasA[int]) is not None, "safe cast to a matching protocol returns the value"
assert (D() cast? HasA[str]) is None, "safe cast to a non-matching protocol yields None"

# a method-bearing protocol has no runtime residue, so the checked cast degrades
# to an unchecked pass-through (it must never raise a `TypeError` from an
# `isinstance` against the protocol)
class HasGet[T](Protocol):
    def get(self) -> T: ...

assert (C() cast HasGet[int]) is not None, "method-protocol cast degrades, never raises"

print("ok")
"#;

/// Locate a usable 3.13 interpreter: `$PYTHON` first, then common names.
/// Returns `None` (test skips) when none is found — PEP 695 class syntax is a
/// hard requirement here.
fn python() -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("PYTHON") {
        candidates.push(p);
    }
    candidates.extend(["python3.13", "python3"].map(String::from));

    candidates.into_iter().find(|py| {
        Command::new(py)
            .args(["-c", "type X[T] = T"])
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
fn parametric_protocol_checks_run_correctly() {
    let Some(python) = python() else {
        eprintln!("skipping parametric-is runtime test: no PEP 695-capable interpreter found");
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
        "transpiled parametric-is program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
