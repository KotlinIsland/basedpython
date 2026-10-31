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
//! Each program is transpiled for the interpreter it runs on, so below 3.12 and
//! 3.13 its generics reach the PEP 695 and PEP 696 polyfills. A reified type
//! parameter is the exception: the transpiler refuses one below 3.12, so the case
//! that needs one is [`REIFIED_CELL_PROGRAM`], and an older interpreter asserts the
//! refusal instead of running it.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;
use interpreters::Interpreter;

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

/// a value typed by a *reified* type parameter, which carries the answer in a runtime cell
const REIFIED_CELL_PROGRAM: &str = r#"
# the cast is exact — the same `T == int` lowering `is` uses
def cast_cell[T](data: list[T]) -> list[int] | None:
    return data cast? list[int]

assert cast_cell([1, 2]) is not None, "reified cell says int"
assert cast_cell(["a"]) is None, "reified cell says str"

print("ok")
"#;

/// the pep 696 half: a parameter's default, which the probe reads in place of an argument
/// the value was never given
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

/// generic classes whose bases fix or reorder the arguments, which the probe of a value typed
/// `object` resolves down the declared base chain. below 3.12 the pep 695 polyfill declares the parameters through
/// `Generic[...]`, so the chain is read from what that records
const BASE_CHAIN_PROGRAM: &str = r#"
class Odd[T](list[int]): ...

class Swap[A, B](dict[B, A]): ...

def is_list_int(x: object) -> bool:
    return x is list[int]

def is_list_str(x: object) -> bool:
    return x is list[str]

def is_dict_str_int(x: object) -> bool:
    return x is dict[str, int]

def is_dict_int_str(x: object) -> bool:
    return x is dict[int, str]

assert is_list_int(Odd[str]()), "the base fixes `int`"
assert not is_list_str(Odd[str]()), "`T` is not list's argument"
assert is_dict_str_int(Swap[int, str]()), "the base reorders"
assert not is_dict_int_str(Swap[int, str]()), "and not the other way round"

print("ok")
"#;

/// transpile `program` for `python` and run its module-level `assert`s on it
fn run_program(python: &Interpreter, program: &str) {
    run_program_for(python, program, python.version);
}

const PICKLED_SPECIALIZATIONS_PROGRAM: &str = r#"
import pickle

class Box[T]:
    def __init__(self, t: T):
        self.t = t

def forwards[T](t: T) -> object:
    return Box(t)

def untyped(x):
    return forwards(x)

unrecorded = pickle.loads(pickle.dumps(untyped(1)))
assert unrecorded.t == 1
assert not unrecorded is Box[int], "a value with no type argument records nothing"

literal = pickle.loads(pickle.dumps(forwards(1)))
assert literal is Box[1], "a literal type argument survives pickling"
assert not literal is Box[2]
print("ok")
"#;

/// a pep 695 type parameter can't be pickled, and a literal type argument isn't a module-level
/// name, so both need saying how they pickle
#[test]
fn a_recorded_specialization_can_be_pickled() {
    let Some(python) = common::python(PythonVersion::PY313) else {
        return;
    };
    run_program(&python, PICKLED_SPECIALIZATIONS_PROGRAM);
}

const STRING_LITERAL_IN_AN_F_STRING_PROGRAM: &str = r#"
class B[out T]:
    def __init__(self, t: T):
        self.t = t

# `build` checks its result without forwarding `T`, which a target below 3.12 refuses
def build[T](t: T) -> B[T]:
    return make(t)

def make(t):
    return B(t)

assert f"{build('x').t}" == "x"
assert f'{B("y").t}' == "y"
assert f"{B('x') is B['x']}" == "True"
assert f"{B['x']('x').t}" == "x"
assert f"{list['x, y']}" == "list[typing.Literal['x, y']]"
assert f"{B(b'x') is B[b'x']}" == "True"
print("ok")
"#;

/// below 3.12 a replacement field can hold neither a backslash nor the f-string's own quote, and
/// a string literal can reach one from the soundness check of a call as well as a construction
#[test]
fn a_string_literal_type_argument_is_written_for_an_f_string_below_3_12() {
    let Some(python) = common::python(PythonVersion::PY311) else {
        return;
    };
    run_program_for(
        &python,
        STRING_LITERAL_IN_AN_F_STRING_PROGRAM,
        PythonVersion::PY311,
    );
}

const VARIANCE_FIXTURE: &str = r#"
from collections.abc import Sequence
from typing import Any, Never

class Out[out T]: pass
class In[in T]: pass
class Box[T]:
    value: T
class Child[T](Out[T]): pass
class Inf[T]:
    def get(self) -> T:
        raise NotImplementedError
class Takes[T]:
    def put(self, value: T) -> None:
        pass
class Seq[T](Sequence[T]):
    def __len__(self) -> int:
        return 0
    def __getitem__(self, index):
        return self

def v(value: object) -> object:
    return value
"#;

/// every target of [`VARIANCE_FIXTURE`] written where the test is, which any target can run
const DIRECT_VARIANCE_PROGRAM: &str = r#"
assert v(Out[int]()) is Out[object]
assert v(In[object]()) is In[int]
assert v(Out[Out[int]]()) is Out[Out[object]]
assert v(Out[In[object]]()) is Out[In[int]]
assert v(Out[int]()) is Out[int | str]
assert v(Out[list[int]]()) is Out[Sequence[int]]
assert v(Out[list[bool]]()) is Out[Sequence[int]]
assert v(Out[int]()) is Out[Any]
assert v(Box[int]()) is Box[out object]
assert v(Child[int]()) is Out[object]
assert v(Out[tuple[int]]()) is Out[tuple[object]]
assert v(Out[None]()) is Out[int | None]
assert not v(Out[object]()) is Out[int]
assert not v(Box[int]()) is Box[object]

# variance the checker inferred, and the variance of a class of the standard library
assert v(Inf[int]()) is Inf[object]
assert not v(Inf[object]()) is Inf[int]
assert v(Takes[object]()) is Takes[int]
assert not v(Takes[int]()) is Takes[object]
assert v(Seq[int]()) is Sequence[object]
assert not v(Seq[object]()) is Sequence[int]
assert v(Out[Seq[int]]()) is Out[Sequence[object]]
assert not v(Out[Seq[object]]()) is Out[Sequence[int]]

# the same position read the other way round flips it
assert v(In[Out[object]]()) is In[Out[int]]
assert not v(In[Out[int]]()) is In[Out[object]]
assert v(In[Sequence[object]]()) is In[list[int]]

# a projection belongs to its argument, wherever that argument is
assert v(Out[Box[int]]()) is Out[Box[out object]]
assert not v(Out[Box[int]]()) is Out[Box[object]]
assert v(Box[object]()) is Box[in int]
assert not v(Box[int]()) is Box[in object]
assert v(Box[Box[int]]()) is Box[out Box[out object]]
assert not v(Box[Box[object]]()) is Box[out Box[out int]]

# an invariant argument has to be the same type, not the same object
assert v(Box[int | str]()) is Box[str | int]
assert v(Box[bool | int]()) is Box[int]
assert not v(Box[bool | str]()) is Box[int | str]

# a union on the source side has to fit whole
assert v(Out[int | bool]()) is Out[int]
assert not v(Out[int | str]()) is Out[int]
assert v(Out[int | str]()) is Out[object]

# a type argument nothing is known about is compatible with any
assert v(Box[Any]()) is Box[int]
assert v(Out[Any]()) is Out[int]
assert v(Out[list[Any]]()) is Out[list[int]]

# literals
assert v(Out[1]()) is Out[int]
assert v(Out[1]()) is Out[1 | 2]
assert not v(Out[1]()) is Out[2]
assert not v(Out[int]()) is Out[1]
assert not v(Out[True]()) is Out[1]
assert v(Box[bool]()) is Box[True | False]

# `None` is its own type
assert v(Out[None]()) is Out[None]
assert not v(Out[None]()) is Out[int]
assert not v(Out[int]()) is Out[None]

# tuples are covariant in their elements, and have their shape to match
assert v(Out[tuple[int, str]]()) is Out[tuple[object, object]]
assert not v(Out[tuple[int, str]]()) is Out[tuple[object]]
assert v(Out[tuple[int, int]]()) is Out[tuple[int, ...]]
assert v(Out[tuple[bool, ...]]()) is Out[tuple[int, ...]]
assert not v(Out[tuple[int, ...]]()) is Out[tuple[int, int]]
assert not v(Out[tuple[int, str]]()) is Out[tuple[int, ...]]
assert v(Out[tuple[int, str]]()) is Out[tuple]
assert v(Out[tuple[()]]()) is Out[tuple[int, ...]]
assert not v(Out[tuple[()]]()) is Out[tuple[int]]

# a class of the program's maps to the target's arguments through its bases
class Fixed(Out[int]): pass
class Swap[A, B](dict[B, A]): pass
assert v(Out[Fixed]()) is Out[Out[int]]
assert v(Out[Fixed]()) is Out[Out[object]]
assert not v(Out[Fixed]()) is Out[Out[str]]
assert v(Out[Swap[int, str]]()) is Out[dict[str, int]]
assert not v(Out[Swap[int, str]]()) is Out[dict[int, str]]

# `object` takes anything, and a plain class is compared as a class
class Base: pass
class Sub(Base): pass
assert v(Out[Sub]()) is Out[Base]
assert not v(Out[Base]()) is Out[Sub]
assert v(Out[list[int]]()) is Out[object]
print("ok")
"#;

/// [`DIRECT_VARIANCE_PROGRAM`]'s targets through a reified cell, which holds what the call
/// spelled: only what the alias carries can answer
const CELL_VARIANCE_PROGRAM: &str = r#"
def cell[T](value: object) -> bool:
    return value is T

assert cell[Out[object]](Out[int]())
assert cell[In[int]](In[object]())
assert cell[Out[Out[object]]](Out[Out[int]]())
assert cell[Out[In[int]]](Out[In[object]]())
assert cell[Out[int | str]](Out[int]())
assert cell[Out[Sequence[int]]](Out[list[int]]())
assert cell[Out[Sequence[int]]](Out[list[bool]]())
assert cell[Out[Any]](Out[int]())
assert cell[Box[out object]](Box[int]())
assert cell[Out[object]](Child[int]())
assert cell[Out[tuple[object]]](Out[tuple[int]]())
assert cell[Out[int | None]](Out[None]())
assert not cell[Out[int]](Out[object]())
assert not cell[Box[object]](Box[int]())

# variance the checker inferred, and the variance of a class of the standard library
assert cell[Inf[object]](Inf[int]())
assert not cell[Inf[int]](Inf[object]())
assert cell[Takes[int]](Takes[object]())
assert not cell[Takes[object]](Takes[int]())
assert cell[Sequence[object]](Seq[int]())
assert not cell[Sequence[int]](Seq[object]())
assert cell[Out[Sequence[object]]](Out[Seq[int]]())

# a cell is told nothing the class doesn't say: the same arguments spelled in a union, an alias
# or another module's name
X = Out[object]
assert cell[X](Out[int]())
type Y = Out[object]
assert cell[Y](Out[int]())
type Z[A] = Out[A]
assert cell[Z[object]](Out[int]())
assert cell[Out[object] | str](Out[int]())
assert cell[Out[object] | str]("s")
assert not cell[Out[int] | str](Out[object]())
Projected = Box[out object]
assert cell[Projected](Box[int]())
assert not cell[Box[object]](Box[int]())

# a projected and a plain argument are two types, and each is still its own alias
assert Box[out object] == Box[out object]
assert not Box[out object] == Box[object]
assert str(Box[out object]).endswith("Box[out object]")
def holds[T]() -> object:
    return T
assert id(holds[X]()) == id(X)
assert holds[Box[out object]]() == Box[out object]

# a forwarded cell contributes what it holds
def forwarded[T](value: object) -> bool:
    return cell[Out[T]](value)
assert forwarded[object](Out[int]())
assert not forwarded[int](Out[object]())
def built[T](t: T) -> object:
    return Out(t)
class HasInit[out T]:
    def __init__(self, t: T):
        self.t = t
def made[T](t: T) -> object:
    return HasInit(t)
assert cell[HasInit[object]](made(1))
assert cell[HasInit[int]](made(1))
assert not cell[HasInit[str]](made(1))

# `None`, `Any`, a union and a literal in the cell itself
assert cell[None](None)
assert not cell[None](1)
assert cell[Any](1)
assert cell[int | None](None)
assert not cell[int | str](None)
assert cell[1](1)
assert not cell[Never](1)
print("ok")
"#;

/// transpile [`VARIANCE_FIXTURE`] with `program` for `target`, and run it
fn run_variance_program(python: &Interpreter, program: &str, target: PythonVersion) {
    run_program_for(python, &format!("{VARIANCE_FIXTURE}{program}"), target);
}

#[test]
fn a_type_argument_relates_as_the_checker_says_written_where_the_test_is() {
    let Some(python) = common::python(PythonVersion::PY310) else {
        return;
    };
    run_variance_program(&python, DIRECT_VARIANCE_PROGRAM, python.version);
    // below 3.12 the class is the polyfill's, and says what it varies by all the same
    if python.version >= PythonVersion::PY312 {
        run_variance_program(&python, DIRECT_VARIANCE_PROGRAM, PythonVersion::PY311);
    }
}

#[test]
fn a_type_argument_relates_as_the_checker_says_through_a_reified_cell() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_variance_program(&python, CELL_VARIANCE_PROGRAM, python.version);
}

const UNDECIDABLE_PROGRAM: &str = r#"
from collections.abc import Callable, Iterable, Iterator
from typing import NamedTuple, NewType, Protocol, TypedDict, runtime_checkable

class Out[out T]: pass

def v(value: object) -> object:
    return value

def cell[T](value: object) -> bool:
    return value is T

def raises_type_error(call) -> bool:
    try:
        call()
    except TypeError:
        return True
    return False

class Proto(Protocol):
    def method(self) -> int: ...
class Conforms:
    def method(self) -> int:
        return 1
class Declares(Proto):
    def method(self) -> int:
        return 1
@runtime_checkable
class Checkable(Protocol):
    def method(self) -> int: ...
class WrongSignature:
    def method(self) -> str:
        return ""

# a protocol no `issubclass` can read: nothing shows the relation but a declared base
assert not v(Out[Conforms]()) is Out[Proto]
assert v(Out[Declares]()) is Out[Proto]
# a runtime checkable one answers as `isinstance` does, by the members being there
assert v(Out[Conforms]()) is Out[Checkable]
assert v(Out[WrongSignature]()) is Out[Checkable]
# in the cell itself, `isinstance` refuses it
assert raises_type_error(lambda: cell[Proto](Conforms()))

class Movie(TypedDict):
    name: str
class Sequel(Movie):
    number: int
assert v(Out[Sequel]()) is Out[Movie]
assert v(Out[Movie]()) is Out[Movie]
assert not v(Out[Movie]()) is Out[Sequel]
assert not v(Out[Movie]()) is Out[dict]
assert not v(Out[Movie]()) is Out[dict[str, object]]
assert raises_type_error(lambda: cell[Movie]({"name": "a"}))

# a callable type relates to another by its signature, and to nothing else
assert v(Out[(object) -> bool]()) is Out[(int) -> int]
assert not v(Out[(int) -> int]()) is Out[(object) -> bool]
assert not v(Out[Conforms]()) is Out[(int) -> int]
assert v(Out[(int) -> int]()) is Out[Callable]
# a value records no signature
assert not cell[(int) -> int](lambda x: x)
assert cell[Callable](lambda x: x)

class Point(NamedTuple):
    x: int
    y: str
assert v(Out[Point]()) is Out[tuple[int, str]]
assert v(Out[Point]()) is Out[tuple[object, ...]]
assert not v(Out[Point]()) is Out[tuple[str, str]]

Id = NewType("Id", int)
assert v(Out[Id]()) is Out[int]
assert not v(Out[int]()) is Out[Id]

# a class of the standard library whose stub fixes its arguments, which the runtime can't see
assert not v(Out[str]()) is Out[Iterable[str]]
# or that only has the methods of an abstract class, which says nothing about its arguments
assert not v(Out[enumerate[int]]()) is Out[Iterator[int]]
assert not v(Out[enumerate[int]]()) is Out[Iterator[tuple[int, int]]]
print("ok")
"#;

/// what the runtime has no way of reading says no, or refuses as `isinstance` does, rather than
/// a `True` the checker wouldn't give
#[test]
fn what_the_runtime_cannot_decide_never_answers_a_true_the_checker_would_not() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, UNDECIDABLE_PROGRAM);
}

const EXPLICIT_LITERAL_PROGRAM: &str = r#"
from enum import Enum
from typing import Literal
import pickle

class Color(Enum):
    RED = 1
    BLUE = 2

class B[T]:
    def __init__(self, t: T):
        self.t = t

class Reads[T]:
    def read(self) -> object:
        return T

class Builds[T]:
    def make(self) -> object:
        if isinstance(T, type):
            return T()
        return None

class Method:
    def test[T](self, x: object) -> bool:
        return x is T

def cell[T](x: object) -> bool:
    return x is T

def read[T]() -> object:
    return T

def inferred[T](t: T) -> object:
    return T

def build[T]() -> object:
    if isinstance(T, type):
        return T()
    return None

def same(explicit: object, solved: object) -> bool:
    return explicit == solved and id(type(explicit)) == id(type(solved))

# what an explicit argument records is what the same argument solved from a call records
assert same(read[1](), inferred(1))
assert same(read["a"](), inferred("a"))
assert same(read[True](), inferred(True))
assert same(read[-1](), inferred(-1))
assert same(read[b"x"](), inferred(b"x"))
assert same(read[None](), inferred(None))
assert same(read[Color.RED](), inferred(Color.RED))
assert same(read[1.5](), inferred(1.5))
assert same(Reads[1]().read(), inferred(1))
assert same(Reads["a"]().read(), inferred("a"))
assert same(Reads[Literal[1]]().read(), inferred(1))
assert same(read[Literal[1]](), inferred(1))

# which reads as a type wherever a body reads it
assert cell[1](1)
assert not cell[1](2)
assert not cell[1](True)
assert cell["a"]("a")
assert cell[True](True)
assert not cell[True](1)
assert cell[-1](-1)
assert cell[b"x"](b"x")
assert cell[1 | 2](2)
assert not cell[1 | 2](3)
assert cell["a" | None](None)
assert cell["a" | "b"]("b")
assert cell[Color.RED](Color.RED)
assert not cell[Color.RED](Color.BLUE)
assert cell[Literal[1]](1)
assert cell[Literal[1, 2]](2)
assert not cell[Literal[1]](2)
assert build[1]() == 1
assert Builds[1]().make() == 1
assert id(pickle.loads(pickle.dumps(read[1]()))) == id(read[1]())

# nested, and on a method, and held on to
assert cell[B[1]](B[1](1))
assert not cell[B[2]](B[1](1))
assert not cell[B[int]](B[1](1))
assert same(read[B[1]](), read[B[1]]())
assert same(Reads[B[1]]().read(), read[B[1]]())
assert cell[list[1]] is not None
assert Method().test[1](1)
assert not Method().test[1](2)
specialized = cell[1]
assert specialized(1)
assert not specialized(2)
Alias = Reads[1]
assert same(Alias().read(), inferred(1))

# written by name
class Named[X, Y]:
    def read(self) -> object:
        return (X, Y)
assert Named[1, Y="a"]().read() == (read[1](), read["a"]())
def pack[**Fields]() -> object:
    return Fields
assert pack[a=1]() == {"a": read[1]()}

# as the target of a test, and read in a replacement field
def target(x: object) -> tuple[bool, bool, bool]:
    return (x is Reads[1], x is B[1], x is Reads[B[1]])
assert target(Reads[1]()) == (True, False, False)
assert target(B[1](1)) == (False, True, False)
assert target(Reads[B[1]]()) == (False, False, True)
assert f"{cell[1](1)} {cell['a']('a')}" == "True True"
print("ok")
"#;

#[test]
fn an_explicit_literal_type_argument_records_what_an_inferred_one_does() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, EXPLICIT_LITERAL_PROGRAM);
}

const UNTRANSPILED_CALLER_MODULE: &str = r#"
from collections.abc import Sequence

class Out[out T]: pass
class Inf[T]:
    def get(self) -> T:
        raise NotImplementedError
class Box[T]:
    value: T

def cell[T](value: object) -> bool:
    return value is T

TOLD = Sequence[object]
"#;

const UNTRANSPILED_CALLER: &str = r#"
from collections.abc import Iterable, Sequence
from typing import Generic, TypeVar
from transpiled import Box, Inf, Out, TOLD, cell

# a class a transpiled module defines told the runtime when it was defined, whoever asks
assert cell[Out[object]](Out[int]())
assert cell[Inf[object]](Inf[int]())
assert not cell[Out[int]](Out[object]())
assert not cell[Box[object]](Box[int]())

T_co = TypeVar("T_co", covariant=True)
class Legacy(Generic[T_co]): pass
class Native[T]:
    def get(self) -> T:
        raise NotImplementedError

# a class no transpiled module defines was told nothing unless one of them subscripted it. what
# it declares is what there is to read, and a pep 695 parameter declares no variance
assert cell[Legacy[object]](Legacy[int]())
assert not cell[Legacy[int]](Legacy[object]())
assert not cell[Native[object]](Native[int]())
assert cell[Native[int]](Native[int]())

class Seq(Sequence[int]):
    def __len__(self): return 0
    def __getitem__(self, index): return self
class Iter(Iterable[int]):
    def __iter__(self): return iter(())
assert cell[TOLD](Seq())
assert not cell[Iterable[object]](Iter())
assert cell[Iterable[int]](Iter())
print("ok")
"#;

/// a module nothing transpiled still calls into what was transpiled
#[test]
fn an_untranspiled_caller_gets_what_the_runtime_was_told() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    let config = Config {
        min_version: python.version,
        ..Config::default()
    };
    let transpiled =
        transpile(UNTRANSPILED_CALLER_MODULE, &config).expect("transpile should succeed");
    let directory = tempfile::tempdir().expect("failed to create a directory");
    std::fs::write(directory.path().join("transpiled.py"), transpiled)
        .expect("failed to write the transpiled module");
    let output = Command::new(&python.command)
        .current_dir(directory.path())
        .arg("-c")
        .arg(UNTRANSPILED_CALLER)
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

/// transpile `program` for `min_version` and run its module-level `assert`s on `python`
fn run_program_for(python: &Interpreter, program: &str, min_version: PythonVersion) {
    let config = Config {
        min_version,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");

    let output = Command::new(&python.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled parametric-is program failed on {python} at {min_version}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn parametric_protocol_checks_run_correctly() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    run_program(&python, PROGRAM);
}

#[test]
fn a_reified_cell_makes_a_cast_exact() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    // the reified cell is a pep 695 closure cell, which needs native type-parameter syntax
    if python.version < PythonVersion::PY312 {
        let config = Config {
            min_version: python.version,
            ..Config::default()
        };
        let refusal = transpile(REIFIED_CELL_PROGRAM, &config)
            .expect_err("a reified generic is refused below python 3.12");
        assert!(
            refusal.contains("requires python 3.12 or newer"),
            "{refusal}"
        );
        return;
    }
    run_program(&python, REIFIED_CELL_PROGRAM);
}

#[test]
fn every_target_kind_checks_correctly_at_runtime() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    run_program(&python, TARGETS_PROGRAM);
}

#[test]
fn type_param_defaults_resolve_at_runtime() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    run_program(&python, DEFAULTS_PROGRAM);
}

#[test]
fn a_base_chain_resolves_on_native_and_polyfilled_parameters() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    // the interpreter's own version declares the parameters natively from 3.12, and 3.11 is
    // the newest version that still polyfills them
    for target in [python.version, PythonVersion::PY311] {
        if target <= python.version {
            run_program_for(&python, BASE_CHAIN_PROGRAM, target);
        }
    }
}

/// a reified class needs 3.13
#[test]
fn a_check_written_from_a_float_type_checks_just_float() {
    let Some(python) = common::python(PythonVersion::PY313) else {
        return;
    };
    run_program(&python, JUST_FLOAT_PROGRAM);
}

#[test]
fn a_target_another_lowering_rewrites_checks_correctly_at_runtime() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    run_program(&python, LOWERED_TARGETS_PROGRAM);
}
