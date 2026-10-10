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

# a default re-evaluated per call has no value before the call, so nothing at runtime
# records the type the checker reads off it — the marker standing in the signature
# is not that type. the check refuses to answer rather than answer from the marker
class FeedLate:
    def f(self, other = C()):
        pass

def refused(test) -> bool:
    try:
        test()
    except TypeError as error:
        return "per call" in str(error)
    return False

def feeds_bool(value: object) -> bool:
    return value is Feed[bool]

assert refused(lambda: FeedLate() is Feed[C]), "the check refuses the type the default would give"
assert refused(lambda: feeds_bool(FeedLate())), "and one it would not"

# an impl with an extra *required* parameter the protocol doesn't supply can't
# satisfy it — a protocol-shaped call would fail to provide that argument
class ExtraRequired:
    def f(self, other = True, *, extra):
        pass

assert (ExtraRequired() is Feed[bool]) is False, "extra required parameter fails"

# so does a required one after a default, which python cannot declare and which stands
# in the signature as a marker in place of a default
class ExtraRequiredAfterDefault:
    def f(self, other = True, extra):
        pass

assert feeds_bool(ExtraRequiredAfterDefault()) is False, "a required parameter after a default fails"

# a required parameter after a default the protocol does supply is required, not a
# default evaluated per call: an unannotated one says nothing about its type, so the
# check answers `False` rather than refusing
class FeedSecond[in out T](Protocol):
    def f(self, a: int, other: T): ...

class ReqAfterDefault:
    def f(self, a = 1, other):
        pass

def feeds_second_bool(value: object) -> bool:
    return value is FeedSecond[bool]

assert feeds_second_bool(ReqAfterDefault()) is False, "a required parameter is not a late-bound default"

# a class of the program's own may carry every attribute a marker has, and a default
# that is one of its instances is still a value, read by its type: a marker is only the
# object its module binds under the name it carries. written in plain python, so the
# default stands in the signature as written
_plain = {"__name__": "__main__"}
exec("""
class Tagged:
    _by_marker = True
    _by_name = "Tagged"

class Forged:
    _by_name = "Forged"
    _by_source = None

class TakesTagged:
    def f(self, other=Tagged()):
        pass

class TakesForged:
    def f(self, other: bool = True, extra=Forged()):
        pass
""", _plain)
Tagged = _plain["Tagged"]
Forged = _plain["Forged"]

assert feeds_bool(_plain["TakesTagged"]()) is False, "a value with a marker's attributes is a value"
assert feeds_bool(_plain["TakesForged"]()) is True, "and a default, not a required parameter's marker"

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
/// basedpython whose module-level `assert`s exercise the runtime check each
/// *non-parametric* target lowers to. The checker's own tests pin the type it
/// reads and the transpiler's pin the text it emits; only running the result
/// says whether the check accepts the values the type contains and no others.
const TARGETS_PROGRAM: &str = r#"
from typing import Callable, Literal, Protocol, runtime_checkable

class A: ...
class B(A): ...

# a class is the `isinstance` it looks like
assert (B() is A) is True, "a subclass instance is an A"
assert (A() is B) is False, "a base instance is not a B"

# `type[C]` asks two things, and the runtime can answer both
assert (B is type[A]) is True, "a subclass is a type[A]"
assert (B() is type[A]) is False, "an instance is not a type[A]"
assert (int is type[A]) is False, "an unrelated class is not a type[A]"

# a literal names the values equal to it, with the class pinned: `1 == True` in
# python, and `Literal[1]` does not contain `True`
def literal_one(v: object) -> bool:
    return v is Literal[1]

assert literal_one(1) is True, "1 is a Literal[1]"
assert literal_one(True) is False, "True is not a Literal[1]"
assert literal_one(2) is False, "2 is not a Literal[1]"

def literal_true(v: object) -> bool:
    return v is Literal[True]

assert literal_true(True) is True, "True is a Literal[True]"
assert literal_true(1) is False, "1 is not a Literal[True]"

# a union holds when any arm does, including one the source spells with a
# single word
type Key = int | str

def key(v: object) -> bool:
    return v is Key

assert key(1) is True, "an int is a Key"
assert key("a") is True, "a str is a Key"
assert key(1.5) is False, "a float is not a Key"

# a template literal type is the set of strings its pattern produces
def item(s: str) -> bool:
    return s is f"item-{int}"

assert item("item-12") is True, "item-12 matches"
assert item("item--3") is True, "a negative renders with its sign"
assert item("item-0") is True, "zero renders as one digit"
assert item("item--0") is False, "str(-0) is 0, so -0 renders no such string"
assert item("item-01") is False, "a leading zero is not an int rendering"
assert item("item-ab") is False, "a non-numeric tail does not match"
assert item("item-") is False, "an empty hole does not match"
assert (1 is f"item-{int}") is False, "a non-str is never one of the strings"

# a pattern's literal text matches itself, metacharacters and all
def dotted(s: str) -> bool:
    return s is f"a.b{int}"

assert dotted("a.b1") is True, "the dot is literal text"
assert dotted("axb1") is False, "and does not stand for any character"

# `None` is a value, so its test is the identity python already performs
def optional(v: object) -> bool:
    return v is None

assert optional(None) is True, "None is None"
assert optional(0) is False, "0 is not None"

# a bare `Callable` asks exactly what `callable()` answers
def call(v: object) -> bool:
    return v is Callable

assert call(len) is True, "a builtin is callable"
assert call(A) is True, "a class is callable"
assert call(1) is False, "an int is not"

# a `@runtime_checkable` protocol is the check python itself performs
@runtime_checkable
class HasName(Protocol):
    name: str

class Named:
    def __init__(self) -> None:
        self.name = "x"

assert (Named() is HasName) is True, "a value with the member satisfies it"
assert (1 is HasName) is False, "one without does not"

# `is not` is the negation, evaluated once
calls = []

def once() -> object:
    calls.append(1)
    return 1

assert (once() is not Literal[2]) is True, "the negation holds"
assert len(calls) == 1, "an effectful value is evaluated once"

print("ok")
"#;

/// basedpython whose module-level `assert`s run what the transpiler writes out from the
/// checker's spelling of a type that holds a `float`. `float` in basedpython is just `float`,
/// not python's `int | float`, so a type test's probe compares the arguments a `list[float]`
/// subclass records against `float`, and a reified type parameter is handed `float` itself
const JUST_FLOAT_PROGRAM: &str = r#"
# a type test names the alias, so the probe is rebuilt from the alias's value
type Floats = list[float]

class FloatList(list[float]): ...
class IntList(list[int]): ...

def floats(v: object) -> bool:
    return v is Floats

assert floats(FloatList()) is True, "a list subclass of float is a list[float]"
assert floats(IntList()) is False, "a list subclass of int is not"
assert (FloatList() cast? Floats) is not None, "a checked cast accepts a list subclass of float"
assert (IntList() cast? Floats) is None, "and refuses a list subclass of int"

# a reified type parameter is handed `float` itself
class Tagged[reified T]:
    def __init__(self, t: T):
        self.t = t

    def argument(self) -> object:
        return T

def argument[reified T](x: T) -> object:
    return T

def from_variable(y: float) -> object:
    return argument(y)

def from_floats(ys: list[float]) -> object:
    return argument(ys)

def from_complex(c: complex) -> object:
    return argument(c)

class Made: ...

assert argument(1.5) === float, f"a float literal reifies as {argument(1.5)}"
assert from_variable(1.5) === float, f"a float variable reifies as {from_variable(1.5)}"
assert from_floats([1.5]) == list[float], f"a list of floats reifies as {from_floats([1.5])}"
assert from_complex(1j) === complex, f"a complex variable reifies as {from_complex(1j)}"
# a constructed value is exactly a `Made`, and the argument is the class
assert argument(Made()) === Made, f"a constructed value reifies as {argument(Made())}"
assert Tagged(1.5).argument() === float, f"a float literal specializes as {Tagged(1.5).argument()}"

print("ok")
"#;

/// basedpython whose module-level `assert`s run checked casts and type tests against targets
/// another lowering rewrites where they are written — `float`, `int?`, a literal, a tuple
/// type, `typeof`, a callable type, `dynamic` and the rest. the check is built from the
/// checker's reading of the target, so it tests what the target means rather than how it
/// is spelled; and where the operand's type cannot hold the target, the checker settles the
/// check without the target at all
const LOWERED_TARGETS_PROGRAM: &str = r#"
y = 1

def raises(f: () -> object) -> bool:
    try:
        f()
    except TypeError:
        return True
    return False

# the operand's type cannot hold the target, so the checker settles the cast without the
# target: it never succeeds
def int_to_float(x: int) -> float:
    return x cast! float

def int_to_float_or_none(x: int) -> float?:
    return x cast? float

assert raises(lambda: int_to_float(3)), "an int is not a float"
assert int_to_float_or_none(3) is None, "an int is not a float"

def str_to_one(x: str) -> object:
    return x cast? 1

def str_to_pair(x: str) -> object:
    return x cast? (int, str)

def str_to_named(x: str) -> object:
    return x cast? (a: int, b: str)

def str_to_inf(x: str) -> object:
    return x cast? float.inf

def str_to_floats(x: str) -> object:
    return x cast? tuple[float, ...]

assert str_to_one("1") is None
assert str_to_pair("a") is None
assert str_to_named("a") is None
assert str_to_inf("inf") is None
assert str_to_floats("a") is None

# the operand may hold the target, so the check is built from the checker's reading of it
def to_float(x: object) -> float:
    return x cast! float

assert to_float(3.5) == 3.5
assert raises(lambda: to_float(3)), "an int is not a float"

def to_optional(x: object) -> int?:
    return x cast! int?

assert to_optional(None) is None
assert to_optional(1) == 1
assert raises(lambda: to_optional("a"))

class FloatList(list[float]): ...
class IntList(list[int]): ...

def to_pair(x: object) -> object:
    return x cast? (int, str)

def to_floats(x: object) -> object:
    return x cast? list[float]

def to_y(x: object) -> object:
    return x cast? typeof y

def to_y_plus_one(x: object) -> object:
    return x cast? typeof y + 1

def to_float_class(x: object) -> object:
    return x cast? type[float]

def to_pattern(x: object) -> object:
    return x cast? f"a{int}b"

def to_dynamic_list(x: object) -> object:
    return x cast? list[dynamic]

def to_callbacks(x: object) -> object:
    return x cast? list[(int) -> str]

assert to_pair((1, "a")) is not None
assert to_pair([1, "a"]) is None
assert to_floats(FloatList()) is not None
assert to_floats(IntList()) is None
assert to_y(1) == 1
assert to_y(2) is None
assert to_y(True) is None
assert to_y_plus_one(2) == 2
assert to_y_plus_one(1) is None
assert to_float_class(float) === float
assert to_float_class(int) is None
assert to_pattern("a1b") == "a1b"
assert to_pattern("a1c") is None
assert to_dynamic_list([1]) == [1]
assert to_dynamic_list((1,)) is None
assert to_callbacks([str]) is not None
assert to_callbacks("s") is None

# a type test is built the same way, and `float` is just `float` there too
def is_float(v: object) -> bool:
    return v is float

assert is_float(3.5) is True
assert is_float(3) is False, "an int is not a float"

def is_floats(v: object) -> bool:
    return v is list[float]

def is_y(v: object) -> bool:
    return v is typeof y

def is_callbacks(v: object) -> bool:
    return v is list[(int) -> str]

def is_dynamic_list(v: object) -> bool:
    return v is list[dynamic]

def str_is_named(v: str) -> bool:
    return v is (a: int, b: str)

assert is_floats(FloatList()) is True
assert is_floats(IntList()) is False
assert is_y(1) is True
assert is_y(2) is False
assert is_callbacks([]) is False
assert is_dynamic_list([1]) is True
assert is_dynamic_list((1,)) is False
assert str_is_named("a") is False

print("ok")
"#;

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
fn every_target_kind_checks_correctly_at_runtime() {
    let Some(python) = python_supporting(PEP695_PROBE) else {
        eprintln!("skipping type-test target runtime test: no PEP 695-capable interpreter found");
        return;
    };
    run_program(&python, TARGETS_PROGRAM);
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

/// a reified class needs 3.13, so this is probed like [`DEFAULTS_PROGRAM`]
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_check_written_from_a_float_type_checks_just_float() {
    let Some(python) = python_supporting(PEP696_PROBE) else {
        eprintln!("skipping just-float runtime test: no 3.13+ interpreter found");
        return;
    };
    run_program(&python, JUST_FLOAT_PROGRAM);
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_target_another_lowering_rewrites_checks_correctly_at_runtime() {
    let Some(python) = python_supporting(PEP695_PROBE) else {
        eprintln!("skipping lowered-target runtime test: no PEP 695-capable interpreter found");
        return;
    };
    run_program(&python, LOWERED_TARGETS_PROGRAM);
}
