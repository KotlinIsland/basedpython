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
class Empty: pass

# a protocol is related to by the members of the class, of the types its annotations say
assert v(Out[Conforms]()) is Out[Proto]
assert v(Out[Declares]()) is Out[Proto]
assert not v(Out[WrongSignature]()) is Out[Proto]
assert not v(Out[Empty]()) is Out[Proto]
assert v(Out[Conforms]()) is Out[Checkable]
assert not v(Out[WrongSignature]()) is Out[Checkable]
# in the cell itself it answers as the target written in place does, which for a runtime checkable
# protocol is `isinstance`, by the members being there
assert cell[Proto](Conforms())
assert cell[Proto](Declares())
assert not cell[Proto](WrongSignature())
assert not cell[Proto](Empty())
assert cell[Checkable](Conforms())
assert cell[Checkable](WrongSignature())

class Movie(TypedDict):
    name: str
class Sequel(Movie):
    number: int
assert v(Out[Sequel]()) is Out[Movie]
assert v(Out[Movie]()) is Out[Movie]
assert not v(Out[Movie]()) is Out[Sequel]
assert not v(Out[Movie]()) is Out[dict]
assert not v(Out[Movie]()) is Out[dict[str, object]]
assert not cell[Movie]({"name": "a"})

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

# a class that only has the methods of an abstract class, which says nothing about its arguments
assert not v(Out[enumerate[int]]()) is Out[Iterator[int]]
assert not v(Out[enumerate[int]]()) is Out[Iterator[tuple[int, int]]]
print("ok")
"#;

/// what the runtime has no way of reading says no, rather than a `True` the checker wouldn't give
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
assert build[1]() is None
assert Builds[1]().make() is None
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
from collections.abc import AsyncIterable, Iterable, Sequence
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
class Aiter(AsyncIterable[int]):
    def __aiter__(self): return self
assert cell[TOLD](Seq())
# told with `Sequence`, which derives from it
assert cell[Iterable[object]](Iter())
assert cell[Iterable[int]](Iter())
assert not cell[AsyncIterable[object]](Aiter())
assert cell[AsyncIterable[int]](Aiter())
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

    let output = common::run_source(&mut Command::new(&python.command), &transpiled);

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

const A_LITERAL_IS_NOT_A_CLASS_PROGRAM: &str = r#"
from enum import Enum
import copy
import pickle
from typing import Literal

class E(Enum):
    A = 1
    B = 2

def read[T]() -> object:
    return T

def inferred[T](t: T) -> object:
    return T

def is_class[T]() -> bool:
    return isinstance(T, type)

def constructed[T: int](x: T) -> object:
    if isinstance(T, type):
        return T(x + 1)
    return "not a class"

def members[T: E]() -> object:
    if isinstance(T, type):
        return [member for member in T]
    return "not a class"

def is_str[T](x: T) -> bool:
    return isinstance(T, type) and issubclass(T, str)

def made[T: int]() -> object:
    if isinstance(T, type):
        return T.__new__(T)
    return "not a class"

def cell[T](x: object) -> bool:
    return x is T

class Box[T]:
    def read(self) -> object:
        return T

# a literal is a type argument and not a class, so it fails the guard wherever it came from
assert not is_class[1]()
assert not is_class["a"]()
assert not is_class[E.A]()
assert not is_class[1 | 2]()
assert is_class[int]()
assert constructed(1) == "not a class"
assert constructed[int](1) == 2
assert members[E.A]() == "not a class"
assert members[E]() == [E.A, E.B]
assert not is_str("a")
assert is_str[str]("a")
assert made[1]() == "not a class"

# what it is, is what `typing` calls it
assert read[1]() == Literal[1]
assert repr(read[1]()) == "typing.Literal[1]"
assert repr(Box[1]().read()) == "typing.Literal[1]"
assert id(read[1]()) == id(inferred(1))
assert id(read["a"]()) == id(inferred("a"))
assert read[1 | 2]() == Literal[1] | Literal[2]
assert read[True]() != read[1]()

# and `x is T` still asks whether `x` is that value
assert cell[1](1)
assert not cell[1](True)
assert not cell[1](2)
assert cell[E.A](E.A)
assert not cell[E.A](E.B)
assert cell[b"x"](b"x")

# nothing about it is left on an instance
class F[T]:
    pass
assert "__by_values__" not in vars(F[1]())
assert id(pickle.loads(pickle.dumps(read[1]()))) == id(read[1]()) or pickle.loads(pickle.dumps(read[1]())) == read[1]()
assert copy.deepcopy(read[1]()) == read[1]()
print("ok")
"#;

#[test]
fn a_literal_type_argument_is_not_a_class() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, A_LITERAL_IS_NOT_A_CLASS_PROGRAM);
}

const SUBSTITUTED_THROUGH_BASES_PROGRAM: &str = r#"
from typing import get_args

class Out[out T]: pass

def chk[T](x: object) -> bool:
    return x is T

class A[T]:
    def t(self) -> object:
        return T
    @classmethod
    def ct(cls) -> object:
        return T
    def holds(self, x: object) -> bool:
        return chk[T](x)

class C[U](A[list[U]]): pass
class D(C[str]): pass
class F[U](A[(U, U)]): pass
class G[U](A[Out[U]]): pass
class H[U](A[U | None]): pass
class I[U](A[(U) -> int]): pass
class J[U, V](A[dict[U, list[V]]]): pass
class K[U](C[Out[U]]): pass

assert C[int]().t() == list[int]
assert C[int].ct() == list[int]
assert D().t() == list[str]
assert F[int]().t() == tuple[int, int]
assert G[bool]().holds(Out[bool]())
assert G[bool]().holds(Out[int]()) is False
assert H[int]().t() == (int | None)
assert get_args(I[str]().t()) == ([str], int)
assert J[str, int]().t() == dict[str, list[int]]
assert K[int]().t() == list[Out[int]]

# a class that answers a subscript for itself is asked nothing while one is rebuilt
class Own[out V]:
    def __class_getitem__(cls, item: object) -> object:
        return ("mine", item)
class W[U](A[Own[U]]): pass
assert id(W[int]().t().__origin__) == id(Own)
assert W[int]().t().__args__ == (int,)

# a parameter of the subclass may share its name with the one it fills in
class Same[T](A[list[T]]):
    def own(self) -> object:
        return T
assert Same[int]().t() == list[int]
assert Same[int]().own() == int

# a deeper chain that reuses the name as well
class Deeper[T](Same[Out[T]]):
    def deepest(self) -> object:
        return T
assert Deeper[int]().t() == list[Out[int]]
assert Deeper[int]().own() == Out[int]
assert Deeper[int]().deepest() == int
print("ok")
"#;

/// a reified base's argument may be built from the parameters of the class that derives it
#[test]
fn a_type_argument_is_substituted_through_reified_bases() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, SUBSTITUTED_THROUGH_BASES_PROGRAM);
}

const FORWARD_REFERENCE_ARGUMENT_PROGRAM: &str = r#"
class Out[out T]: pass
def chk[T](x: object) -> bool:
    return x is T

class Other: pass
x = Out[Later]()
class S(Out[Later]): pass
y = list[Later]
class L(y): pass
z = Out[list[Later]]()
class Unrelated: pass
class Later(Other): pass

assert chk[Out[Later]](x)
assert chk[Out[Other]](x)
assert not chk[Out[Unrelated]](x)
assert not chk[Out[int]](x)
assert chk[Out[Later]](S())
assert chk[Out[Other]](S())
assert not chk[Out[Unrelated]](S())
assert chk[list[Later]](L())
assert not chk[list[Other]](L())
assert chk[Out[list[Later]]](z)
assert x is Out[Later]
assert x is Out[Other]
assert not x is Out[Unrelated]
print("ok")
"#;

/// a type argument python evaluates before the class it names exists is read once it does
#[test]
fn a_forward_reference_type_argument_relates_as_the_class_it_names() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, FORWARD_REFERENCE_ARGUMENT_PROGRAM);
}

const STUB_RELATION_PROGRAM: &str = r#"
from collections.abc import Callable, Collection, Iterable, Mapping, MutableSequence, Sequence, Set
from typing import Hashable, LiteralString, NamedTuple, Sized, SupportsInt, TypedDict
import numbers

class Out[out T]: pass
class In[in T]: pass
class A: pass
class C: pass
class D(A, C): pass
class TD(TypedDict):
    a: int
class NT(NamedTuple):
    a: int
class IntS(Sequence[int]): pass

def chk[T](x: object) -> bool:
    return x is T

# a class of the standard library has the bases its stub gives it
okay: Out[Sequence[str]] = Out[str]()
assert chk[Out[Sequence[str]]](Out[str]())
assert chk[Out[Sequence[object]]](Out[str]())
assert chk[Out[Iterable[object]]](Out[str]())
assert not chk[Out[Sequence[int]]](Out[str]())
assert chk[Out[Sequence[str]]](Out["a"]())
assert chk[Out[Sequence[int]]](Out[bytes]())
assert chk[Out[Collection[int]]](Out[bytearray]())
assert chk[Out[Iterable[int]]](Out[memoryview]())
assert not chk[Out[Sequence[str]]](Out[bytes]())
assert chk[Out[Iterable[object]]](Out[dict[str, int]]())
assert chk[Out[Iterable[str]]](Out[dict[str, int]]())
assert not chk[Out[Iterable[int]]](Out[dict[str, int]]())
assert chk[Out[Iterable[object]]](Out[Mapping[str, int]]())
assert chk[Out[MutableSequence[int]]](Out[list[int]]())
assert not chk[Out[MutableSequence[int]]](Out[list[bool]]())
assert chk[Out[Sequence[int]]](Out[list[bool]]())
assert chk[Out[Sequence[int]]](Out[IntS]())
assert chk[Out[Iterable[int]]](Out[IntS]())
assert not chk[Out[Iterable[str]]](Out[IntS]())
assert chk[In[str]](In[Sequence[str]]())

# the element type of a tuple is what its base is specialized with
assert chk[Out[Sequence[object]]](Out[(int, str)]())
assert chk[Out[Sequence[int | str]]](Out[(int, str)]())
assert not chk[Out[Sequence[int]]](Out[(int, str)]())
assert chk[Out[Sequence[int]]](Out[tuple[int, ...]]())
assert chk[Out[Collection[int]]](Out[tuple[bool, ...]]())
assert chk[Out[Sequence[int]]](Out[NT]())
assert not chk[Out[Sequence[str]]](Out[NT]())

# a `TypedDict` is a `Mapping[str, object]`, and no `dict`
assert chk[Out[Mapping[str, object]]](Out[TD]())
assert chk[Out[Iterable[object]]](Out[TD]())
assert chk[Out[Sized]](Out[TD]())
assert not chk[Out[Mapping[str, int]]](Out[TD]())
assert not chk[Out[dict[str, object]]](Out[TD]())

# what the stub doesn't say the runtime's registrations don't say either
assert not chk[Out[numbers.Number]](Out[int]())
assert not chk[Out[numbers.Number]](Out[1]())
assert not chk[Out[numbers.Number]](Out[float]())

# a protocol has the members it declares, and nothing an object happens to have
assert not chk[Out[Hashable]](Out[Iterable[int]]())
assert not chk[Out[Hashable]](Out[Sized]())
assert not chk[Out[Hashable]](Out[SupportsInt]())
assert chk[Out[Sized]](Out[Collection[int]]())
assert chk[Out[Hashable]](Out[int]())
assert not chk[Out[Hashable]](Out[list[int]]())

# a class's type parameter is covariant in `type`, and `type` itself is `type[object]`
assert not chk[Out[type[int]]](Out[type]())
assert chk[Out[type[object]]](Out[type]())
assert chk[Out[type]](Out[type[int]]())
assert chk[Out[type[int]]](Out[type[bool]]())

# an intersection holds what all of its members hold
assert chk[Out[A & C]](Out[D]())
assert chk[Out[A]](Out[A & C]())
assert chk[Out[C]](Out[A & C]())
assert not chk[Out[A & C]](Out[A]())
assert not chk[Out[D]](Out[A & C]())
assert chk[A & C](D())
assert not chk[A & C](A())

# a class is called as its constructor says
assert chk[Out[(bool) -> object]](Out[type[int]]())
assert chk[Out[() -> int]](Out[type[int]]())
assert chk[Out[(...) -> int]](Out[type[bool]]())
assert chk[Out[() -> bool]](Out[type[bool]]())
assert not chk[Out[() -> bool]](Out[type[int]]())
assert not chk[Out[(object) -> int]](Out[type[int]]())
assert chk[Out[(int) -> str]](Out[type]())
assert chk[Out[(int) -> str]](Out[type[object]]())
print("ok")
"#;

/// the bases a stub gives a class are in no runtime object, so the checker says what they are
#[test]
fn a_stub_class_relates_as_its_stub_says() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, STUB_RELATION_PROGRAM);
}

const TYPE_CELL_PROGRAM: &str = r#"
from enum import Enum

class A: pass
class B(A): pass
class Color(Enum):
    RED = 1

def chk[T](x: object) -> bool:
    return x is T

def a(x: object) -> bool:
    return x is type[A]
def b(x: object) -> bool:
    return x is type[object]
def i(x: object) -> bool:
    return x is type[int]
def u(x: object) -> bool:
    return x is type[int | str]

values: list[object] = [A, B, int, bool, str, type, Color, A(), 1, None, list[int]]
for value in values:
    assert a(value) == chk[type[A]](value), value
    assert b(value) == chk[type[object]](value), value
    assert i(value) == chk[type[int]](value), value
    assert u(value) == chk[type[int | str]](value), value

assert chk[type[A]](B)
assert chk[type[A]](A)
assert not chk[type[B]](A)
assert not chk[type[A]](A())
assert chk[type[int]](int)
assert chk[type[int]](bool)
assert chk[type[int | str]](str)
assert chk[type[object]](type)
assert chk[type[Color]](Color)
print("ok")
"#;

/// the docs promise `x is T` answers what the same target written in place does
#[test]
fn a_cell_holding_a_type_of_a_class_answers_as_the_target_in_place_does() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, TYPE_CELL_PROGRAM);
}

const UNUSED_PARAMETER_PROGRAM: &str = r#"
class P[T]: pass
class Private[T]:
    private x: T
    def __init__(self, x: T):
        self.x = x

def chk[T](x: object) -> bool:
    return x is T

# the checker takes a parameter no member mentions for a covariant one
assert chk[P[object]](P[bool]())
assert not chk[P[bool]](P[object]())
assert P[bool]() is P[object]
assert not P[object]() is P[bool]
# and one only a private member mentions for a bivariant one
assert chk[Private[object]](Private[bool](True))
assert chk[Private[bool]](Private[object](True))
print("ok")
"#;

#[test]
fn an_unused_type_parameter_relates_as_the_checker_says() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, UNUSED_PARAMETER_PROGRAM);
}

const PACKS_AND_SPECIAL_FORMS_PROGRAM: &str = r#"
from collections.abc import Sequence
from typing import LiteralString, Self

class V[*Ts]: pass
class M[A, *Ts, B]: pass
class Out[out T]: pass

def chk[T](x: object) -> bool:
    return x is T

# a tuple records no arguments, but its elements are there to ask
assert chk[()](())
assert not chk[()]((1,))
assert chk[(int, str)]((1, "a"))
assert not chk[(int, str)](("a", 1))
assert not chk[(int, str)]((1,))
assert chk[tuple[int, ...]](())
assert chk[tuple[int, ...]]((1, True))
assert not chk[tuple[int, ...]]((1, "a"))
assert chk[(int, *tuple[str, ...], bool)]((1, "a", "b", True))
assert not chk[(int, *tuple[str, ...], bool)]((1, "a", 2, True))
assert chk[(1, "a")]((1, "a"))
assert not chk[(1, "a")]((2, "a"))
assert chk[Sequence[bool]]((True,))
assert chk[Sequence[bool]](())
assert not chk[Sequence[bool]]((1,))

assert chk[V[()]](V[()]())
assert not chk[V[()]](V[int]())
assert not chk[V[int]](V[()]())
assert chk[V[int, str]](V[int, str]())
assert chk[V[*tuple[int, ...]]](V[int, int]())
assert chk[V[*tuple[int, ...]]](V[()]())
assert chk[V[*tuple[int, ...]]](V[bool, int]())
assert not chk[V[*tuple[int, ...]]](V[int, str]())
assert not chk[V[int, int]](V[*tuple[int, ...]]())
assert chk[V[object, *tuple[int, ...]]](V[str, int, bool]())
assert chk[M[int, str, bool]](M[int, str, bool]())
assert chk[M[int, *tuple[str, ...], bool]](M[int, str, str, bool]())
assert not chk[M[int, *tuple[str, ...], bool]](M[int, str, int, bool]())

assert chk[Out[str]](Out[LiteralString]())
assert chk[Out[LiteralString]](Out["a"]())
assert chk[Out[LiteralString]](Out[LiteralString]())
assert chk[Out[object]](Out[LiteralString]())
assert not chk[Out[LiteralString]](Out[str]())
assert not chk[Out[LiteralString]](Out[1]())
assert not chk[Out[int]](Out[LiteralString]())

class K:
    def m(self) -> bool:
        return chk[Out[K]](Out[Self]())
    def other(self) -> bool:
        return chk[Out[Sub]](Out[Self]())
    @classmethod
    def c(cls) -> bool:
        return chk[Out[K]](Out[Self]())
    def nested(self) -> bool:
        def inner() -> bool:
            return chk[Out[K]](Out[Self]())
        return inner()
class Sub(K): pass
class Unrelated: pass
assert K().m()
assert Sub().m()
assert not K().other()
assert Sub().other()
assert K.c()
assert K().nested()
print("ok")
"#;

#[test]
fn a_pack_a_literal_string_and_self_relate_as_the_checker_says() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, PACKS_AND_SPECIAL_FORMS_PROGRAM);
}

const PICKLED_REIFIED_PROGRAM: &str = r#"
import copy
import pickle

class R[T]:
    def __init__(self, n: int = 0):
        self.n = n
    def t(self) -> object:
        return T
class S[T]:
    __slots__ = ("n",)
    def __init__(self):
        self.n = 1
    def t(self) -> object:
        return T
class D(R[str]): pass
class Reduces[T]:
    def __init__(self, n: int):
        self.n = n
    def __reduce__(self):
        return (type(self), (self.n,))
    def t(self) -> object:
        return T

def f[T]() -> object:
    return T
class H:
    def m[T](self) -> object:
        return T

def round_trip[V](value: V) -> V:
    return pickle.loads(pickle.dumps(value))

for protocol in range(pickle.HIGHEST_PROTOCOL + 1):
    made = pickle.loads(pickle.dumps(R[int](3), protocol))
    assert made.t() == int
    assert made.n == 3
    assert id(type(made)) == id(R[int])
    if protocol >= 2:
        assert pickle.loads(pickle.dumps(S[int](), protocol)).n == 1
    assert id(pickle.loads(pickle.dumps(f, protocol))) == id(f)
    assert pickle.loads(pickle.dumps(f[int], protocol))() == int

assert round_trip(D(4)).t() == str
assert round_trip(D(4)).n == 4
assert repr(round_trip(R[1]()).t()) == "typing.Literal[1]"
assert round_trip(R[list[int]]()).t() == list[int]
assert id(round_trip(R[R[int]]()).t()) == id(R[int])
assert round_trip(R[list[R[int]]]()).t() == list[R[int]]
assert round_trip(Reduces[int](2)).n == 2
assert round_trip(Reduces[int](2)).t() == int
assert round_trip(H().m[int])() == int

# `copy` still works
assert copy.copy(R[int](5)).t() == int
assert copy.copy(R[int](5)).n == 5
assert copy.deepcopy(R[int](5)).t() == int
assert copy.copy(Reduces[int](5)).t() == int
assert copy.deepcopy(Reduces[int](5)).t() == int
print("ok")
"#;

/// `pickle` looks a class up by name, and no name holds a specialization
#[test]
fn a_specialized_instance_and_a_reified_function_can_be_pickled() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, PICKLED_REIFIED_PROGRAM);
}

const BASES_SIDE_BY_SIDE_PROGRAM: &str = r#"
class Mix[T]:
    def m(self) -> object:
        return T
class Other[U]:
    def o(self) -> object:
        return U
class Same[T]:
    def s(self) -> object:
        return T
class Twin[T]:
    def t(self) -> object:
        return T
class B(Mix[int], Other[str]): pass
class C(Same[int], Twin[str]): pass
class D(B): pass
assert B().m() == int
assert B().o() == str
assert C().s() == int
assert C().t() == str
assert D().o() == str
print("ok")
"#;

/// a plain subclass is what each of its specialized bases makes it, not only the first
#[test]
fn a_class_deriving_two_specializations_reads_each() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, BASES_SIDE_BY_SIDE_PROGRAM);
}

const TYPING_ALIASES_PROGRAM: &str = r#"
import typing
from typing import AbstractSet, Container, FrozenSet, Iterable, Mapping, Reversible, Sequence

class Out[out T]: pass

def chk[T](x: object) -> bool:
    return x is T

assert chk[Out[Sequence[object]]](Out[Sequence[int]]())
assert not chk[Out[Sequence[int]]](Out[Sequence[object]]())
assert chk[Out[Iterable[object]]](Out[Iterable[int]]())
assert chk[Out[Mapping[str, object]]](Out[Mapping[str, int]]())
assert chk[Out[FrozenSet[object]]](Out[FrozenSet[int]]())
assert chk[Out[AbstractSet[object]]](Out[AbstractSet[int]]())
assert chk[Out[Container[object]]](Out[Container[int]]())
assert chk[Out[Reversible[object]]](Out[Reversible[int]]())

class IntS(typing.Sequence[int]):
    def __len__(self) -> int:
        return 0
    def __getitem__(self, index: int) -> int:
        return 0
assert chk[Sequence[object]](IntS())
assert chk[Sequence[int]](IntS())
print("ok")
"#;

/// `typing` names a class of `collections.abc` by an alias of it
#[test]
fn a_class_written_through_typing_is_the_class() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, TYPING_ALIASES_PROGRAM);
}

const RACING_FIRST_USE_PROGRAM: &str = r#"
import sys
import threading
from collections.abc import Container, Iterable, Mapping, Sequence

class Out[out T]: pass

def chk[T](x: object) -> bool:
    return x is T

sys.setswitchinterval(1e-6)
errors: list[BaseException] = []
barrier = threading.Barrier(16)

def race() -> None:
    barrier.wait()
    try:
        for _ in range(20):
            assert chk[Out[Sequence[str]]](Out[str]())
            assert chk[Out[Container[object]]](Out[Mapping[str, int]]())
            assert chk[Out[Iterable[object]]](Out[dict[str, int]]())
    except BaseException as error:
        errors.append(error)

threads = [threading.Thread(target=race) for _ in range(16)]
for thread in threads:
    thread.start()
for thread in threads:
    thread.join()
assert errors == [], errors
print("ok")
"#;

/// what the runtime is told is settled by whichever thread asks first
#[test]
fn threads_racing_on_the_first_test_agree() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, RACING_FIRST_USE_PROGRAM);
}

const STRUCTURAL_STUB_PROTOCOL_PROGRAM: &str = r#"
from collections.abc import Iterable, Iterator
from typing import IO, Protocol, SupportsAbs, TextIO, Hashable
import io
import types

class Out[out T]: pass

def chk[T](x: object) -> bool:
    return x is T

class MyIt:
    def __iter__(self) -> Iterator[int]:
        return iter([1])
class StrIt:
    def __iter__(self) -> Iterator[str]:
        return iter([""])
class NoIt: pass

ok: Out[Iterable[int]] = Out[MyIt]()
assert chk[Out[Iterable[int]]](ok)
assert chk[Out[Iterable[object]]](Out[MyIt]())
assert not chk[Out[Iterable[int]]](Out[StrIt]())
assert not chk[Out[Iterable[int]]](Out[NoIt]())

class Mine[T](Protocol):
    def __iter__(self) -> Iterator[T]: ...
assert chk[Out[Mine[int]]](Out[MyIt]())
assert not chk[Out[Mine[str]]](Out[MyIt]())

# a class of a stub against a protocol of a stub
assert chk[Out[SupportsAbs[int]]](Out[int]())
assert not chk[Out[SupportsAbs[str]]](Out[int]())
assert chk[Out[Iterable[str]]](Out[io.StringIO]())
assert not chk[Out[Iterable[int]]](Out[io.StringIO]())
assert chk[Out[Iterable[str]]](Out[IO[str]]())
# typeshed gives `IO.__iter__` a type parameter of its own, which any call can solve to anything
assert chk[Out[Iterable[int]]](Out[IO[str]]())

# the stub says it is no such thing, whatever the runtime class has
assert not chk[Out[Hashable]](Out[types.MappingProxyType[str, int]]())
print("ok")
"#;

/// a protocol of a stub has members of the types the stub says, for a class that has them
#[test]
fn a_protocol_of_a_stub_is_related_to_by_its_members() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, STRUCTURAL_STUB_PROTOCOL_PROGRAM);
}

const CONSTRUCTORS_FROM_ANNOTATIONS_PROGRAM: &str = r#"
from typing import Literal

class Out[out T]: pass

def chk[T](x: object) -> bool:
    return x is T

class D:
    def __init__(self, a: Literal["x"]): pass
class E:
    def __init__(self, a: Literal[":x"]): pass
assert chk[Out[(Literal["x"]) -> D]](Out[type[D]]())
assert chk[Out[(Literal[":x"]) -> E]](Out[type[E]]())
assert not chk[Out[(Literal["y"]) -> E]](Out[type[E]]())
assert not chk[Out[() -> E]](Out[type[E]]())

# a class of a function
def local() -> None:
    class L:
        def __init__(self, x: int): pass
    ok: Out[(int) -> L] = Out[type[L]]()
    assert chk[Out[(int) -> L]](ok)
    assert not chk[Out[(str) -> L]](ok)
local()

# a class bound twice is called as each one says
class A:
    def __init__(self, x: int): pass
first = chk[Out[(int) -> A]](Out[type[A]]())
class A:  # type: ignore
    def __init__(self, x: str): pass
second = chk[Out[(str) -> A]](Out[type[A]]())
assert first and second
print("ok")
"#;

/// a class whose definition runs says what calling it is by its annotations
#[test]
fn a_class_is_called_as_its_annotations_say() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, CONSTRUCTORS_FROM_ANNOTATIONS_PROGRAM);
}

const DEPTH_AND_HOSTILE_CLASSES_PROGRAM: &str = r#"
class Out[out T]: pass

def chk[T](x: object) -> bool:
    return x is T

class M(type):
    def __hash__(cls) -> int:
        raise ValueError("no hash")
class G[out T](metaclass=M): pass
assert G.__name__ == "G"
print("ok")
"#;

#[test]
fn a_metaclass_that_refuses_to_hash_its_class_runs_as_it_does_in_python() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, DEPTH_AND_HOSTILE_CLASSES_PROGRAM);
}

/// a type nested further than any bound a relation could have been given
#[test]
fn a_type_nested_deep_relates_as_a_shallow_one_does() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    let nested =
        |depth: usize, leaf: &str| format!("{}{leaf}{}", "Out[".repeat(depth), "]".repeat(depth));
    for depth in [70, 150] {
        let program = format!(
            "class Out[out T]: pass\ndef chk[T](x: object) -> bool:\n    return x is T\n\
             assert chk[{}]({}())\n\
             assert chk[{}]({}())\n\
             assert not chk[{}]({}())\nprint(\"ok\")\n",
            nested(depth, "object"),
            nested(depth, "int"),
            nested(depth, "int"),
            nested(depth, "int"),
            nested(depth, "int"),
            nested(depth, "object"),
        );
        // `by` lowers a program on a thread of this size, and this nesting needs it where
        // nothing inlines, a test's own thread having a fraction of that
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(ruff_db::STACK_SIZE)
                .spawn_scoped(scope, || run_program(&python, &program))
                .expect("failed to start the worker thread")
                .join()
                .expect("the program should run");
        });
    }
}

const FORWARD_REFERENCES_THAT_HOLD_PROGRAM: &str = r#"
import copy
import pickle

class Inv[T]: pass
class Out[out T]: pass

def chk[T](x: object) -> bool:
    return x is T

# in a projection
x = Inv[out Later]()
class Later: pass
assert chk[Inv[out Later]](x)
assert chk[Inv[out Later]](Inv[out Later]())
assert not chk[Inv[out int]](x)

# through `pickle`
y = Out[Later2]()
class Later2: pass
assert type(pickle.loads(pickle.dumps(y))).__name__ == "Out"
assert chk[Out[Later2]](pickle.loads(pickle.dumps(y)))
assert chk[Out[Later2]](copy.deepcopy(y))

# of a function, which no module has
def local() -> None:
    y = Out[Local]()
    class Local: pass
    assert chk[Out[Local]](y)
    assert chk[Out[Local]](Out[Local]())
    assert not chk[Out[int]](y)
local()
print("ok")
"#;

/// a forward reference is the class it names wherever it is held
#[test]
fn a_forward_reference_in_a_projection_a_pickle_or_a_function_relates_as_the_class() {
    let Some(python) = common::python(PythonVersion::PY312) else {
        return;
    };
    run_program(&python, FORWARD_REFERENCES_THAT_HOLD_PROGRAM);
}
