//! Runtime divergence test for parametric protocol checks — both `is`-tests
//! (`value is A[int]`) and checked casts (`value cast! A[int]`, `value cast?
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
//! The programs are split by interpreter requirement: [`PROGRAM`] needs only
//! PEP 695 (3.12+), while [`DEFAULTS_PROGRAM`] needs PEP 696 defaults in native
//! syntax and `TypeVar.has_default()` (3.13+). Each is probed for what it
//! actually uses — a 3.12 interpreter parses PEP 695 quite happily, so a probe
//! that only checked that would run the defaults program and die on a
//! `SyntaxError`. Whichever has no capable interpreter skips rather than fails.

use std::process::{Command, Stdio};

use by_transforms::{Config, PythonVersion, transpile};

/// basedpython whose module-level `assert`s exercise the structural protocol
/// check end to end: an exact match, a mismatch, an inferred class-body
/// annotation, a covariant read-only member, a nested-generic member, a missing
/// member, and an inheriting subclass.
const PROGRAM: &str = r#"
from typing import Literal, Never, Protocol

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
assert (C() cast! HasA[bool]) is not None, "checked cast to a matching protocol returns the value"

_raised = False
try:
    D() cast! HasA[str]
except TypeError:
    _raised = True
assert _raised, "checked cast to a non-matching protocol raises"

# `cast?` yields the value on a match and `None` on a mismatch
assert (D() cast? HasA[int]) is not None, "safe cast to a matching protocol returns the value"
assert (D() cast? HasA[str]) is None, "safe cast to a non-matching protocol yields None"

# a *method* member is checked structurally too: its parameters are checked
# contravariantly against the impl method's reified annotations, its return
# covariantly. `in out T` keeps T invariant (and sidesteps the variance error
# that a param-or-return-only bare T would raise)
class Feed[in out T](Protocol):
    def f(self, other: T): ...

class FeedBool:
    # `other = True` gives the parameter an inferred `bool` at runtime
    def f(self, other = True):
        pass

assert (FeedBool() is Feed[int]) is False, "param contravariant: bool does not accept int"
assert (FeedBool() is Feed[bool]) is True, "param matches bool"
assert (C() is Feed[bool]) is False, "value without the method fails"

# an impl with an extra *required* parameter the protocol doesn't supply can't
# satisfy it — a protocol-shaped call would fail to provide that argument
class ExtraRequired:
    def f(self, other = True, *, extra):
        pass

assert (ExtraRequired() is Feed[bool]) is False, "extra required parameter fails"

# a method return is covariant: an impl returning `bool` satisfies a protocol
# asking for `int` (bool <: int), but not one asking for `str`
class Get[in out T](Protocol):
    def get(self) -> T: ...

class GetBool:
    def get(self) -> bool:
        return True

assert (GetBool() is Get[bool]) is True, "return matches bool"
assert (GetBool() is Get[int]) is True, "return covariant: bool <: int"
assert (GetBool() is Get[str]) is False, "return bool is not str"

# a subclass's arguments are resolved *down the declared base chain*, never
# assumed to line up positionally with the base's. `Odd` is a `list[int]`
# whatever `T` is, so `T` must never be reported as list's argument
class Odd[T](list[int]): ...

assert (Odd[str]() is list[int]) is True, "explicit argument, base still fixed"
assert (Odd[str]() is list[str]) is False, "an explicit argument is not the base's either"

# a base may also *reorder* its arguments
class Swap[A, B](dict[B, A]): ...

assert (Swap[int, str]() is dict[str, int]) is True, "the base's order is followed"
assert (Swap[int, str]() is dict[int, str]) is False, "not the subclass's order"

# a base may nest the parameter
class Wrap[T](list[dict[str, T]]): ...

assert (Wrap[int]() is list[dict[str, int]]) is True, "a nested parameter substitutes"
assert (Wrap[int]() is list[dict[str, bool]]) is False, "and stays exact"

# plain (non-generic) inheritance is still followed to the generic base
class Plain(list[int]): ...

class Deeper(Plain): ...

assert (Deeper() is list[int]) is True, "a plain subclass inherits the specialization"

# a *literal* type argument (`A[True]`) specializes the member to
# `Literal[True]`, rebuilt at runtime by `_by_lit`. an invariant data member
# stays exact — a `bool` annotation is not a `Literal[True]`
class Lit[in out T](Protocol):
    a: T

class BoolAttr2:
    a: bool

class TrueAttr:
    a: Literal[True]

assert (BoolAttr2() is Lit[True]) is False, "invariant member: bool is not Literal[True]"
assert (TrueAttr() is Lit[True]) is True, "invariant member: exact literal matches"
assert (TrueAttr() is Lit[False]) is False, "a different literal does not match"

# in a covariant position a literal *is* a subtype of the class of its values
class GetLit[in out T](Protocol):
    def get(self) -> T: ...

class GetTrue:
    def get(self) -> Literal[True]:
        return True

assert (GetTrue() is GetLit[bool]) is True, "Literal[True] <: bool"
assert (GetTrue() is GetLit[int]) is True, "Literal[True] <: int (bool subclasses int)"
assert (GetTrue() is GetLit[str]) is False, "Literal[True] is not a str"
assert (GetTrue() is GetLit[True]) is True, "the same literal matches"

# a method *parameter* is contravariant, so a wider annotation accepts the
# literal the protocol asks for — this is the reported case
class Feed2[in out T](Protocol):
    def f(self, other: T): ...

class FeedBoolAnn:
    def f(self, other: bool):
        pass

assert (FeedBoolAnn() is Feed2[int]) is False, "bool does not accept int"
assert (FeedBoolAnn() is Feed2[bool]) is True, "bool accepts bool"
assert (FeedBoolAnn() is Feed2[True]) is True, "bool accepts Literal[True]"

# a checked cast to a method protocol validates the same claim
assert (GetBool() cast! Get[int]) is not None, "method-protocol cast returns the value on a match"
assert (GetBool() cast? Get[str]) is None, "method-protocol safe cast yields None on a mismatch"

# a union cast is the disjunction of its arms, each checked by its own kind —
# it must never become one `isinstance` against a tuple holding a parameterized
# arm (a runtime TypeError)
assert (GetBool() cast? Get[int] | str) is not None, "generic arm of a union matches"
assert ("s" cast? Get[int] | str) is not None, "plain arm of a union matches"
assert (1 cast? Get[int] | str) is None, "neither arm matches"

# the cast probe is *lenient*: a value recording no reification has no arguments
# to check, so the base class test is the whole guarantee. this keeps a plain
# list castable while still rejecting one whose recorded arguments contradict
assert ([1, 2] cast! list[int]) is not None, "an unreified list still casts"

class IntList(list[int]): ...
class StrList(list[str]): ...

assert (IntList() cast? list[int]) is not None, "recorded arguments agree"
assert (StrList() cast? list[int]) is None, "recorded arguments contradict"

# a value typed by a *reified* type parameter carries the answer in a runtime
# cell, so the cast is exact — the same `T == int` lowering `is` uses
def cast_cell[T](data: list[T]) -> list[int] | None:
    return data cast? list[int]

assert cast_cell([1, 2]) is not None, "reified cell says int"
assert cast_cell(["a"]) is None, "reified cell says str"

# a member declared inside `__init__` has no annotation any runtime can read,
# and the checker accepts the class as satisfying the protocol regardless — so
# answering `False` would be a silent contradiction. the check refuses instead
class Annotated1[T](Protocol):
    slot: T

class ClassLevelSlot:
    slot: int = 1

class InitLevelSlot:
    def __init__(self) -> None:
        self.slot: int = 1

class NoSlot:
    pass

def slot_probe(x: object) -> bool:
    return x is Annotated1[int]

assert slot_probe(ClassLevelSlot()), "a class-level annotation is readable"
assert not slot_probe(NoSlot()), "a genuinely absent member is still `False`"
try:
    slot_probe(InitLevelSlot())
except TypeError as exc:
    assert "declared inside a method" in str(exc), str(exc)
else:
    raise AssertionError("an unreadable member must refuse, not answer `False`")

print("ok")
"#;

/// The pep 696 half, split out because native defaults (`[T = int]`) and the
/// `has_default()` accessor both need 3.13 — a 3.12 interpreter parses the pep
/// 695 syntax in [`PROGRAM`] quite happily, so the two must be probed
/// separately or a 3.12 box runs this and dies on a `SyntaxError`.
const DEFAULTS_PROGRAM: &str = r#"
from typing import Never, Protocol
# a class records its generic bases *unsubstituted* (`class L[T = Never]
# (list[T])` stores `list[T]`), so a type parameter left at its pep 696 default
# must resolve to that default — otherwise the probe compares a bare TypeVar and
# never matches
class DefaultNever[T = Never](list[T]): ...

class DefaultInt[T = int](list[T]): ...

assert (DefaultNever() is list[Never]) is True, "a defaulted parameter resolves to its default"
assert (DefaultNever() is list[int]) is False, "the default is not some other argument"
assert (DefaultInt() is list[int]) is True, "a non-Never default resolves too"

# an *explicit* specialization wins over the default: reading the default over
# the top of it would report an argument the value never had
assert (DefaultInt[str]() is list[str]) is True, "the explicit argument matches"
assert (DefaultInt[str]() is list[int]) is False, "the default must not leak in"

# a parameter with no default stays unknown, so it matches nothing
class NoDefault[T](list[T]): ...

assert (NoDefault() is list[int]) is False, "an unknown argument matches nothing"

# a parameter that appears *only* in the class's own identity is recorded in no
# base at all, so it is read straight off `__type_params__`. `Never` has no
# runtime spelling, so the constructor is left bare — this is exactly the case
# type reification cannot cover
class OwnNever[T = Never]:
    a: T

assert (OwnNever() is OwnNever[Never]) is True, "an own-identity default resolves"
assert (OwnNever() is OwnNever[int]) is False, "and is not some other argument"


print("ok")
"#;

/// Locate a usable 3.13 interpreter: `$PYTHON` first, then common names.
/// Returns `None` (test skips) when none is found — PEP 695 class syntax is a
/// hard requirement here.
/// pep 695 type parameters — what [`PROGRAM`] needs (3.12+)
const PEP695_PROBE: &str = "type X[T] = T";

/// pep 696 defaults in native syntax plus the `has_default()` accessor the probe
/// reads them back through — what [`DEFAULTS_PROGRAM`] needs (3.13+)
const PEP696_PROBE: &str = "\
class _P[T = int](list[T]): pass
assert _P.__type_params__[0].has_default()
";

/// Locate an interpreter satisfying `probe`: `$PYTHON` first, then common names.
/// `None` (test skips) when none qualifies.
fn python_supporting(probe: &str) -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("PYTHON") {
        candidates.push(p);
    }
    candidates.extend(["python3.13", "python3"].map(String::from));

    candidates.into_iter().find(|py| {
        // a rejected probe writes a `SyntaxError` to stderr; swallow it so a
        // passing run's log doesn't look like a failure
        Command::new(py)
            .args(["-c", probe])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// transpile `program` and run its module-level `assert`s on `python`
fn run_program(python: &str, program: &str) {
    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");

    let output = Command::new(python)
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

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn parametric_protocol_checks_run_correctly() {
    let Some(python) = python_supporting(PEP695_PROBE) else {
        eprintln!("skipping parametric-is runtime test: no PEP 695-capable interpreter found");
        return;
    };
    run_program(&python, PROGRAM);
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn type_param_defaults_resolve_at_runtime() {
    let Some(python) = python_supporting(PEP696_PROBE) else {
        eprintln!(
            "skipping parametric-is defaults test: no PEP 696-capable (3.13+) interpreter found"
        );
        return;
    };
    run_program(&python, DEFAULTS_PROGRAM);
}
