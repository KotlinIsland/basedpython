# Cycles

## Recursive lambda in a loop condition

A lambda is always truthy. Determining whether the final assignment is reachable must not require
inferring the lambda's return type, which depends on that same assignment.

```py
(f := lambda: f)
while lambda: f:
    pass
f = 0
```

## Recursive lambda in a conditional

The same cycle can arise when a conditional filters the bindings visible to a recursive lambda.

```py
f = lambda: f
if not (lambda: f):
    f = 0
```

## Function signature

Deferred annotations can result in cycles in resolving a function signature:

```py
from __future__ import annotations

# error: [invalid-type-form]
def f(x: f):
    pass

reveal_type(f)  # revealed: def f(x: Unknown)
```

## Unpacking a recursively growing tuple

This is a regression test for <https://github.com/astral-sh/ty/issues/3838>.

```py
while 1:
    # error: [possibly-unresolved-reference]
    # error: [possibly-unresolved-reference]
    x = (*x, x)

while 1:
    y = (y, *y)
```

## Generic `NamedTuple` with recursive fields

This is a regression test for <https://github.com/astral-sh/ty/issues/3872>. Computing the
`NamedTuple` fields while building the class's MRO must not try to determine whether the same class
is a `TypedDict`.

```toml
[environment]
python-version = "3.14"
```

```py
from typing import NamedTuple

class Node[KT, VT](NamedTuple):
    children: tuple[Node[KT, VT], ...] | tuple[Leaf[VT], ...]

class Leaf[VT](NamedTuple):
    values: tuple[VT, ...]
```

## Literal reduction during cycle recovery

This is a regression test for <https://github.com/astral-sh/ty/issues/3851>. Constructing a union
during cycle recovery must not run redundancy checks between a literal and a protocol instance.
Resolving the protocol interface can depend on the expression inference query that is already being
recovered, which would introduce a new Salsa cycle.

```toml
[environment]
python-version = "3.14"
```

```py
from typing import Protocol, runtime_checkable

_: Any

@property
def prop(self) -> A:
    raise NotImplementedError

@runtime_checkable
class B(Protocol):
    _: A

x = 5

while isinstance(x, B):
    x = B()  # error: [call-non-callable]

type(x)
x = 2

from typing import Any, assert_type

assert_type(prop, property)

if bool:
    x = 5

while isinstance(x, B):
    x = B()  # error: [call-non-callable]

class A: ...
```

## Literal widening during cycle recovery

Once a recursively growing group of integer literals widens to `int`, later iterations must not
reintroduce individual literals. Otherwise, the inferred type continues changing and the cycle never
converges. This is a reduced regression test from SciPy's iterative sparse solvers.

```py
def solve(maxiter, a, b, c, d, e):
    iteration = 0
    stop = 0
    while iteration < maxiter:
        iteration = iteration + 1
        if iteration >= maxiter:
            stop = 7
        if a:
            stop = 6
        if b:
            stop = 5
        if c:
            stop = 4
        if d:
            stop = 3
        if e:
            stop = 2
        if stop > 0:
            break
    return stop
```

## Self-referential bare type alias

```toml
[environment]
python-version = "3.12"  # typing.TypeAliasType
```

```py
from typing import Union, TypeAliasType, Sequence, Mapping

A = list["A | None"]

def f(x: A):
    # TODO: should be `list[A | None]`?
    reveal_type(x)  # revealed: list[Divergent]
    # TODO: should be `A | None`?
    reveal_type(x[0])  # revealed: Divergent

JSONPrimitive = Union[str, int, float, bool, None]
JSONValue = TypeAliasType("JSONValue", 'Union[JSONPrimitive, Sequence["JSONValue"], Mapping[str, "JSONValue"]]')

def _(x: JSONValue):
    reveal_type(x)  # revealed: Sequence[JSONValue] | int | float | None | Mapping[str, JSONValue]
```

## Self-referential legacy type variables

```py
from typing import Generic, TypeVar

B = TypeVar("B", bound="Base")  # error: [missing-type-argument]

class Base(Generic[B]):
    pass
```

## Parameter default values

This is a regression test for <https://github.com/astral-sh/ty/issues/1402>. When a parameter has a
default value that references the callable itself, we currently prevent infinite recursion by simply
falling back to `Unknown` for the type of the default value, which does not have any practical
impact except for the displayed type. We could also consider inferring `Divergent` when we encounter
too many layers of nesting (instead of just one), but that would require a type traversal which
could have performance implications. So for now, we mainly make sure not to panic or stack overflow
for these seemingly rare cases.

### Functions

```py
class C:
    def f(self: "C"):
        def inner_a(positional=self.a):
            return
        self.a = inner_a
        # revealed: def inner_a(positional = ...)
        reveal_type(inner_a)

        def inner_b(*, kw_only=self.b):
            return
        self.b = inner_b
        # revealed: def inner_b(*, kw_only = ...)
        reveal_type(inner_b)

        def inner_c(positional_only=self.c, /):
            return
        self.c = inner_c
        # revealed: def inner_c(positional_only = ..., /)
        reveal_type(inner_c)

        def inner_d(*, kw_only=self.d):
            return
        self.d = inner_d
        # revealed: def inner_d(*, kw_only = ...)
        reveal_type(inner_d)
```

We do, however, still check assignability of the default value to the parameter type:

```py
class D:
    def f(self: "D"):
        # error: [invalid-parameter-default] "Default value of type `(a: int = ...) -> None` is not assignable to annotated parameter type `int`"
        def inner_a(a: int = self.a): ...
        self.a = inner_a
```

### Lambdas

all four show the default one layer deeper than the parameter it is the default of. the two
positional ones used to stop a layer earlier, because the expected type carried into the lambda
folded them back onto the marker — a context holding the cycle's own marker no longer earns a query
key of its own, so they now read the same way the keyword-only two always have:

```py
class C:
    def f(self: "C"):
        self.a = lambda positional=self.a: positional
        self.b = lambda *, kw_only=self.b: kw_only
        self.c = lambda positional_only=self.c, /: positional_only
        self.d = lambda *, kw_only=self.d: kw_only

        # revealed: (positional: (positional: Divergent = ...) -> Divergent = ...) -> Divergent
        reveal_type(self.a)

        # revealed: (*, kw_only: (*, kw_only: Divergent = ...) -> Divergent = ...) -> Divergent
        reveal_type(self.b)

        # revealed: (positional_only: (positional_only: Divergent = ..., /) -> Divergent = ..., /) -> Divergent
        reveal_type(self.c)

        # revealed: (*, kw_only: (*, kw_only: Divergent = ...) -> Divergent = ...) -> Divergent
        reveal_type(self.d)
```

### Self-referential decorated functions

Resolving a decorated function's callable signature must not eagerly infer its default values.
Otherwise, a default that refers back to the decorated name can re-enter the reachability check for
an earlier assertion and prevent inference from converging. This is a regression test for
<https://github.com/astral-sh/ty/issues/4308>.

```py
f = lambda: f
assert f

@property
def f(x=lambda: f): ...
```

The same cycle must converge when the parameter and return type are annotated:

```py
g = lambda: g
assert g

@property
def g(x: object = lambda: g) -> None: ...
```

### Diagnostics for self-referential decorated functions

We reject a decorator that expects an integer instead of a function. Displaying the function's
signature in that diagnostic can infer its self-referential default value. We report the error after
function inference finishes, so diagnostic formatting does not create a cycle through the
reachability check for the earlier assertion. This is a regression test for
<https://github.com/astral-sh/ty/issues/4440>.

```py
def decorator(value: int) -> int:
    return value

def check(value: Recursive):
    reveal_type(value.callback)  # revealed: bound method C[Any].method(*args: Any, **kwargs: Any)
    static_assert(is_subtype_of(TypeOf[value.callback], Callable[[], None]))

f = lambda: f
assert f

# error: [invalid-argument-type] "Expected `int`, found `def f(x=...) -> Unknown`"
@decorator
def f(x=lambda: f): ...
```

### Self-referential property construction

Constructing a property explicitly has the same behavior as decorator syntax:

```py
f = lambda: f
assert f

def getter(x=lambda: f): ...

f = property(getter)
```

### Self-referential callable decorators

The cycle is not specific to properties. A decorator that returns a callable with a fixed signature
must also terminate:

```py
from collections.abc import Callable
from typing import Any

def decorator(fn: Callable[[Any], Any]) -> Callable[[Any], Any]:
    return fn

f = lambda: f
assert f

@decorator
def f(x=lambda: f): ...
```

### Self-referential ParamSpec decorators

A decorator can capture a function's parameters and return a callable with a different signature.
Capturing those parameters must not evaluate a self-referential default.

```toml
[environment]
python-version = "3.12"
```

```py
from collections.abc import Callable

def decorator[**P](fn: Callable[P, None]) -> Callable[[], None]:
    return lambda: None

f = lambda: f
assert f

@decorator
def f(x=lambda: f) -> None: ...

reveal_type(f)  # revealed: () -> None
```

### Self-referential generic properties

A generic getter's annotations are inferred in its type-parameter scope. Constructing the property
must not pull its self-referential default into that inference.

```toml
[environment]
python-version = "3.12"
```

```py
f = lambda: f
assert f

@property
def f[T](value: T, callback=lambda: f) -> T:
    return value

reveal_type(f)  # revealed: property
```

## Decorated methods with implicit class attributes

This is a regression test for <https://github.com/astral-sh/ty/issues/3471>.

```py
from collections.abc import Callable
from typing import TypeVar

class A: ...

T = TypeVar("T")
U = TypeVar("U", bound=A)
C = Callable[[T, U], object]

def d() -> Callable[[C[U, A]], object]:
    raise NotImplementedError

class B:
    @d()
    def m1(self, p):
        pass

    @d()
    def m2(self, p):
        self.__slots__  # error: [unresolved-attribute]
```

## Function annotation and dynamic `NamedTuple` / `NewType`

This is a regression test for <https://github.com/astral-sh/ty/issues/3485> and
<https://github.com/astral-sh/ty/issues/3682>. Type traversal during cycle recovery should not force
the lazy base of a `NewType`.

```py
class C:
    pass

def f():
    pass

def g() -> T:  # error: [unresolved-reference]
    pass

g()

from typing import NamedTuple, NewType

X = NamedTuple("X", [("x", "X")]), None  # error: [invalid-type-form]

list(X)
min(X)  # error: [invalid-argument-type]
T = f()

X = NewType("X", C)
```

The runtime callable returned by `NewType` also carries the lazy base and must use the same
cycle-safe traversal.

```py
class C: ...

def f(): ...
def g() -> T: ...

g()
from typing import NamedTuple, NewType

X = NewType("X", C)
Y = NamedTuple("Y", [("a", "Y")]), X  # error: [invalid-type-form]
min(Y)  # error: [invalid-argument-type]
T = f()
```

## Decorator defined on a base class with constrained typevars, accessed from a subclass with decorated generic parameters

This example was minimized from
[a real issue in `robotframework`](https://github.com/astral-sh/ty/issues/2637#issuecomment-3807037935).
It created
[a complicated cycle with multiple cycle heads](https://gist.github.com/oconnor663/c996ed2cc97d172dd4b9a8d8207dc7ac),
which also involved
[a tricky Salsa behavior that comes up when a query oscillates between being a cycle head and not being one](https://gist.github.com/oconnor663/c2a7662e3d88048b691754da957121d1).

`entry.py`:

```py
from derived import Derived

Derived.decorate
# revealed: bound method <class 'Derived'>.decorate[T](item_class: type[T]) -> type[T]
reveal_type(Derived.decorate)
```

`derived.py`:

```py
from ty_extensions._internal import reveal_mro
import bases

class Derived(bases.GenericBase["Foo", "Bar"]): ...

@Derived.decorate
class Foo(bases.Foo): ...

# revealed: <class 'Foo'>
reveal_type(Foo)
# revealed: (<class 'derived.Foo'>, <class 'bases.Foo'>, <class 'object'>)
reveal_mro(Foo)

@Derived.decorate
class Bar(bases.Bar): ...

# revealed: <class 'Bar'>
reveal_type(Bar)
# revealed: (<class 'derived.Bar'>, <class 'bases.Bar'>, <class 'object'>)
reveal_mro(Bar)
```

`bases.py`:

```py
from typing import Generic, TypeVar, Type
from ty_extensions._internal import reveal_mro

T = TypeVar("T")
B1 = TypeVar("B1", bound="Foo")
B2 = TypeVar("B2", bound="Bar")

class GenericBase(Generic[B1, B2]):
    @classmethod
    def decorate(cls, item_class: Type[T]) -> Type[T]:
        return item_class

# revealed: <class 'GenericBase'>
reveal_type(GenericBase)
# revealed: (<class 'GenericBase[Unknown, Unknown]'>, typing.Generic, <class 'object'>)
reveal_mro(GenericBase)
# revealed: (<class 'GenericBase[Foo, Bar]'>, typing.Generic, <class 'object'>)
reveal_mro(GenericBase["Foo", "Bar"])

class Foo: ...
class Bar: ...
```

## a tuple grown in a loop widens rather than diverging

`key += (x,)` makes the tuple one element longer on every trip, so the fixpoint sees `tuple[T]`,
then `tuple[T] | tuple[T, T]`, then one more union member each iteration — a sequence with no fixed
point, which used to run until salsa gave up. a variable-length tuple *is* one, and it is a
supertype of every member.

```py
def bounded() -> None:
    key = (1,)
    for _ in range(3):
        key += (2,)
    reveal_type(key)  # revealed: tuple[Literal[1, 2], ...]

def unbounded(n: int) -> object:
    key = (1,)
    while True:
        key += (2,)
        if len(key) > n:
            return key

def two_lengths(flag: bool) -> None:
    # *two* tuples of different length in a union is an ordinary type a program can
    # mean, and stays one — only a growing run of them is folded
    pair = (1, 2) if flag else (1, 2, 3)
    reveal_type(pair)  # revealed: tuple[Literal[1], Literal[2]] | tuple[Literal[1], Literal[2], Literal[3]]
```

## a container that nests itself in a loop widens rather than diverging

`stack.append((stack.pop(), a, b))` makes the element type one level *deeper* on every trip, so the
fixpoint sees `list[tuple[T, T, T]]`, then `list[tuple[tuple[T, T, T], T, T]]`, and one more level
each iteration. the type it is reaching for is recursive and cannot be written down, so the
supertype to settle on is the container with its arguments unknown — which is what a checker without
flow-sensitive specializations infers for the same loop.

two things had to hold for this to converge. a type built entirely out of `Unknown` never acquires a
divergence marker, so depth is the only signal that it is still growing; and the event timeline of a
fluid specialization has to be *retained* rather than replaced when an iteration produces fewer
events than the last, or the two alternate forever.

```py
def walk(dirs, nondirs, top):
    stack = [top]
    while stack:
        top = stack.pop()
        stack.append((top, dirs, nondirs))

def nested_dict(key: str) -> None:
    seen = {}
    while key:
        seen[key] = seen
        key = key[1:]

def shallow_nesting() -> None:
    # nesting a program actually writes is left alone: this is three levels, not a run
    # that gains one on every trip
    boxed: list[list[list[int]]] = [[[1]]]
    reveal_type(boxed)  # revealed: list[list[list[int]]]
```

## a function whose return value nests itself widens rather than diverging

An unannotated function can hand back a container built out of its own return value. Inferring its
signature then asks what it returns in order to answer what it returns, and each round buries the
answer one level deeper: `Box[Never]`, then `Box[Box[Never]]`, and so on. Nothing in the program
says where that recursion stops, so the rounds never settle.

The recovery is the same as for a container nested in a loop, but the growth has to be recognized
first: by the first round the divergence marker has been solved away — `Box[Divergent]` comes back
as `Box[Never]` — leaving nothing for the usual collapse to find. What gives the round away is that
it added nothing but depth: its answer is the previous answer with one more layer around it. Once
the marker is put back the nesting collapses and the iteration settles on the recursive type.

```toml
[environment]
python-version = "3.12"
```

```py
from typing import Callable

class Box[T]:
    def __init__(self, make: Callable[[], T]) -> None:
        self.make = make

def factory():
    return Box(factory)

reveal_type(factory())  # revealed: Box[Divergent]

# the binding that observes it is one construction further out, and stays finite
outer = Box(factory)
reveal_type(outer)  # revealed: Box[Box[Divergent]]

# an unannotated factory that is not recursive is unaffected, and still inferred exactly
def plain():
    return 1

def holder():
    return Box(plain)

reveal_type(holder())  # revealed: Box[Literal[1]]
```

## the recursive `defaultdict` factory converges

The same shape reaches ty through the standard library, where it is a common idiom for a tree of
arbitrary depth.

```py
from collections import defaultdict

def tree():
    return defaultdict(tree)

reveal_type(tree())  # revealed: defaultdict[Unknown, Divergent]

nested = defaultdict(tree)
reveal_type(nested)  # revealed: defaultdict[Unknown, defaultdict[Unknown, Divergent]]
```

## a recursive return value carried in a tuple element

a recursion routed through a tuple is the same recursion as one written directly, and settles the
same way. it did not, because a tuple is the one container an uninhabited element makes uninhabited:
on the first round that element is the cycle's divergence marker, the marker stands for the least
type there is, and the tuple built around it came back `Never`. the recursive branch then
contributed nothing, the marker was gone, and every later round saw an ordinary string one character
longer than the last — a family of literals that gains a member per round and has no fixed point.

a marker is not a claim that no value exists; it is a type the iteration has not finished computing,
and the tuple has to survive it for the recursion to still be there when recovery folds it back.

```py
def through_tuple(n: int):
    if n:
        return "a"
    t = (through_tuple(n),)
    return "b" + t[0]

reveal_type(through_tuple(1))  # revealed: str

# the same recursion spelled directly and through a list, which always converged
def direct(n: int):
    if n:
        return "a"
    return "b" + direct(n)

reveal_type(direct(1))  # revealed: str

def through_list(n: int):
    if n:
        return "a"
    t = [through_list(n)]
    return "b" + t[0]

reveal_type(through_list(1))  # revealed: str

# a branch that adds nothing to what the other branch already returns stays exact
def unchanged(n: int):
    if n:
        return "a"
    t = (unchanged(n),)
    return t[0]

reveal_type(unchanged(1))  # revealed: Literal["a"]
```

## a recursive return value read back through a parametric context

reading a value back out of a container is the same recursion whichever way it is spelled, so
`next(iter(t))` has to settle where `t[0]` does. it did not, because the two reads take different
paths through a fluid binding: a subscript observes the binding as it was created, while `iter` asks
for an `Iterable[T]` — a context parametric enough that the binding's specialization is solved again
for that use.

that re-solve is driven by the creation type, and it was dropping the creation type on the floor
whenever the only thing in it was the cycle's divergence marker. a marker reads as gradual before it
is materialized and as `Never` once it bottom-materializes, and both of those are exactly what the
re-solve discards as saying nothing about the element. with nothing left to bind the element to, the
use observed `set[Unknown]` where creation had said `set[Divergent]`, and once the marker was gone
recovery had nothing to fold the recursion back onto — so `Unknown` stayed in the answer for good.

which container holds the value has nothing to do with it: a set and a list read back the same way
settle the same way.

```py
def through_set(n: int):
    if n:
        return "a"
    t = {through_set(n)}
    return "b" + next(iter(t))

reveal_type(through_set(1))  # revealed: str

def through_list(n: int):
    if n:
        return "a"
    t = [through_list(n)]
    return "b" + next(iter(t))

reveal_type(through_list(1))  # revealed: str
```

nor does it have to be a container at all. any generic built around the marker and read back through
one of its own type parameters took the same path.

```py
from typing import Generic, TypeVar

T = TypeVar("T")

class Box(Generic[T]):
    def __init__(self, value: T):
        self.value = value

def through_box(n: int):
    if n:
        return "a"
    t = Box(through_box(n))
    return "b" + t.value

reveal_type(through_box(1))  # revealed: str
```

preserving the marker costs no precision elsewhere: a binding built around one and never read back
keeps the literals it was built from, because the marker only ever stood in for the element the
recursion had not settled yet.

```py
def never_read(n: int):
    if n:
        return "a"
    t = {never_read(n)}
    return "b"

reveal_type(never_read(1))  # revealed: Literal["a", "b"]
```

## a recursive value handed to a constructor

`set([h(n)])` is the second way of writing `{h(n)}`, so it has to settle where the display does. it
did not, because building the container by calling its class puts the recursion through two places a
display never reaches, and both of them threw the cycle's divergence marker away.

the first is the constructor's own solve. `set.__init__` takes an `Iterable`, and a protocol formal
is related to its argument through the constraint solver, which reasons about a gradual argument by
its materializations — and a marker's bottom materialization is `Never`. so `Iterable[T]` solved
against `list[Divergent]` learned `Never ≤ T` and the marker was gone before anything else saw it.
reading the same parameter off the argument's own bases keeps it, which is why the identical
constructor declared `list[T]` always settled.

```py
def through_set(n: int):
    if n:
        return "a"
    t = set([through_set(n)])
    return "b" + next(iter(t))

reveal_type(through_set(1))  # revealed: str

def through_frozenset(n: int):
    if n:
        return "a"
    t = frozenset([through_frozenset(n)])
    return "b" + next(iter(t))

reveal_type(through_frozenset(1))  # revealed: str

def through_list(n: int):
    if n:
        return "a"
    t = list([through_list(n)])
    return "b" + next(iter(t))

reveal_type(through_list(1))  # revealed: str
```

nothing about this is particular to the containers typeshed ships. a class of one's own taking an
`Iterable` took the same path, and one taking a `list` never did.

```py
from typing import Generic, Iterable, TypeVar

T = TypeVar("T")

class ViaProtocol(Generic[T]):
    def __init__(self, values: Iterable[T], /) -> None:
        self.first = next(iter(values))

def through_protocol_parameter(n: int):
    if n:
        return "a"
    t = ViaProtocol([through_protocol_parameter(n)])
    return "b" + t.first

reveal_type(through_protocol_parameter(1))  # revealed: str
```

the marker only ever stood in for the element the recursion had not settled yet, so keeping it costs
nothing where there was never a recursion to settle.

```py
def literal_element(n: int):
    return set(["a"])

reveal_type(literal_element(1))  # revealed: set[str]
```

## a recursive value read back through a call on the marker

the round that builds the binding is the round in which the binding's own definition is still being
computed, so the name reads as the bare marker there and reading it back is a call *on* the marker.
such a call used to answer `Unknown` twice over: the marker bound none of the callee's typevars, and
where several overloads matched it the menu of possible results collapsed to a gradual type.

that `Unknown` does not stay where it was produced. it is recorded as the element type of the list
literal the binding was built from — a query of its own, which the return type's discard of its
first rounds never revisits — so every later round reads it back and the recursion settles on
`str | Unknown`.

```py
def through_dunder_iter(n: int):
    if n:
        return "a"
    t = list([through_dunder_iter(n)])
    return "b" + next(t.__iter__())

reveal_type(through_dunder_iter(1))  # revealed: str

def through_second_container(n: int):
    if n:
        return "a"
    t = list([through_second_container(n)])
    return "b" + list(t)[0]

reveal_type(through_second_container(1))  # revealed: str
```

a marker is what the answer is *not yet*, so a call on one answers with the marker. a genuinely
gradual argument still answers gradually, since there is no fixed point on its way.

```py
from typing import Any

def gradual_argument(x: Any):
    reveal_type(iter(x))  # revealed: Unknown
    reveal_type(set([x]))  # revealed: set[Any]
```

## a statement call whose callee is still being inferred

a call on a line of its own is asked whether it returns before anything after it is checked, because
a call that returns `Never` ends the scope. a method that is still having its own signature inferred
has no return type yet, and the placeholder standing in for it until then returns `Never` — so on
that round the call reads as terminal and the rest of `seek` is unreachable. that is what settles
`seek`, which settles `record`, which makes the same call read as returning, and the round after
that starts again from the placeholder.

neither reading ever repeats the one before it, so the search for a fixed point has none to find.
the round that saw the call return is the one that stands, and every later round of the same cycle
keeps it.

the types are read from inside a method rather than from the module: reading them from the module
asks for `seek`'s signature before the class body is checked, which is not the order that reaches
the cycle at all.

```toml
[environment]
python-version = "3.13"
```

```py
class Reader:
    def reset(self):
        self.decoder = None

    def record(self, chars):
        self.used = 0

    def seek(self, cookie):
        self.record("")
        self.decoder = self.reset()
        self.record(self.decoder)
        self.used = cookie

    def check(self):
        # nothing in either body ends the scope, so control reaches the end of both
        reveal_type(self.seek(1))  # revealed: None
        reveal_type(self.reset())  # revealed: None

        reveal_type(self.decoder)  # revealed: None | Unknown
        reveal_type(self.used)  # revealed: int | cookie@seek
```

## an attribute rebuilt out of its own elements

one method seeds an attribute with a fixed-length tuple and another rebuilds it out of an element it
reads back out, so the attribute is defined in terms of itself. both bindings are the same length,
so the widening that gives up a *growing* length has nothing to give up — but the rebuilt tuple
still has an element standing for the cycle, and an element standing for the cycle is exactly what
the divergence marker replaces. the widened form and the marked form each undo the other, so unless
the widening is handed back through the marker rather than around it the two alternate with period
two and neither is ever reached.

reading the element back out is what makes the shape: `self.t = (self.t,)` nests instead, and
settles on its own.

the signatures are left uninferred because an inferred one reaches the attribute through the
method's return type as well, and a second route into the cycle changes which query is its head —
the shape under test is the one the attribute makes on its own:

```toml
[analysis]
infer-unannotated-signatures = false
```

```py
class Subscript:
    def g(self):
        self.t = (1,)

    def f(self):
        self.t = (self.t[0],)
        reveal_type(self.t)  # revealed: tuple[Divergent]

class Unpacked:
    def g(self):
        self.t = (1,)

    def f(self):
        (a,) = self.t
        self.t = (a,)
        reveal_type(self.t)  # revealed: tuple[Divergent]

class Starred:
    def g(self):
        self.t = (1,)

    def f(self):
        self.t = (*self.t,)
        reveal_type(self.t)  # revealed: tuple[Divergent, ...]

class Nested:
    def g(self):
        self.t = ((1,),)

    def f(self):
        self.t = ((self.t[0][0],),)
        reveal_type(self.t)  # revealed: tuple[tuple[Divergent]]
```

## a type context that is the cycle's own marker

two attributes each rebuilt out of the other's elements reach a fixed point in a handful of rounds —
and then keep going. the query an annotated inference runs under is interned on the expected type,
so each round's expected type interns a key of its own, and each key brings a fresh divergence
marker named after it. the marker names the query, the query is named by the key, and the key holds
the marker, so the round count is the only thing still moving.

what the marker says is that the cycle has not reached a type yet, which is no guidance, so a
context holding one is dropped and the expression is inferred bare — the same bare key every round:

```py
class Mutual:
    def g(self):
        self.a = (1,)
        self.b = (2,)

    def f(self):
        self.a = (self.b[0],)
        self.b = (self.a[0],)
        reveal_type(self.a)  # revealed: tuple[Divergent]
        reveal_type(self.b)  # revealed: tuple[Divergent]
```

a `return` is checked against the function's return type, and a `def` that wrote none has one
recovered from this very body — so while that recovery runs, the context a `return` is checked
against is the cycle's own marker. a container built under it read its element type back out of the
context and came back `list[Divergent]`, which the next round reproduced unchanged, so the marker
was what the iteration settled on and what a caller was shown. dropping the context leaves each
round saying what the body actually builds:

```py
def in_return(x: int):
    return [x]

reveal_type(in_return(1))  # revealed: list[int]

# a container reached through a call, and one nested inside another, settle the same way
def through_call(x: int):
    return list([x])

reveal_type(through_call(1))  # revealed: list[int]

def nested(x: int):
    return {x: [x]}

reveal_type(nested(1))  # revealed: dict[int, list[int]]
```

a body that really does nest itself one container deeper per round has no return type to reach, and
keeps the marker that says so:

```py
def endless(x: int):
    return [endless(x)]

reveal_type(endless(1))  # revealed: list[Divergent]
```

## a method that hands back its own bound method

`return self.dispatch` makes `dispatch` return a bound method of `dispatch`, so the type is a cycle
rather than a tree. every walk over it — expanding a signature, comparing two of them, writing one
into a message — arrives back where it started, and each stops at the second visit instead of
following the cycle until the stack runs out.

```toml
[environment]
python-version = "3.13"

[analysis]
sound-types = true
```

```py
class Tracer:
    def dispatch(self, frame):
        return self.dispatch

    def use(self):
        # revealed: bound method Self@use.dispatch(frame) -> bound method Self@use.dispatch(frame) -> bound method Self@use.dispatch(...)
        reveal_type(self.dispatch)

        # a call rebinds the receiver, which maps the signature the cyclic return type sits in
        # revealed: bound method Self@use.dispatch(frame: Literal[1]) -> bound method Self@use.dispatch(frame) -> bound method Self@use.dispatch(...)
        reveal_type(self.dispatch(1))
```

## a self-referential bound method written into a diagnostic

```toml
[environment]
python-version = "3.13"

[analysis]
sound-types = true
```

```py
class Tracer:
    def dispatch(self, frame):
        return self.dispatch

    def use(self) -> int:
        return self.dispatch  # error: [invalid-return-type]
```

## two methods that hand back each other's bound method

each comparison of the cyclic return type against itself asks for a copy of one signature freshened
past the other, so the next round needs one nonce more than the last. no two rounds are ever equal,
which leaves nothing for a memo or a cycle guard to close on — the freshening is bounded instead,
and a pair that runs past the bound is refused rather than pursued.

```toml
[environment]
python-version = "3.13"

[analysis]
sound-types = true
```

```py
class Tracer:
    def trace(self, event):
        if event:
            return self.exception(event)
        return self.trace

    def exception(self, event):
        return self.trace

    def use(self):
        # revealed: bound method Self@use.exception(event) -> bound method Self@use.trace(event) -> (bound method Self@use.trace(event) -> Divergent) | (bound method Self@use.trace(...))
        reveal_type(self.exception)
```

## a tuple grown in a loop, widened while a cycle is being recovered

`args` gains an element every time round the loop, so no round of the fixed-point iteration ever
repeats the one before it. cycle recovery gives the lengths up — nothing in the program says where
the growth stops — and keeps the element type, which the program really does determine.

giving them up means unioning the element types together, and the ordinary union builder simplifies
its elements against one another. those simplifications are relation checks, and a relation check on
a type variable standing for an unannotated parameter answers what that parameter's bound is by
inferring the whole enclosing body — a query the cycle being recovered is already running. salsa
rejects a recovery function that acquires a cycle head of its own, so recovery builds this union
without the simplification.

```py
def f(sequence, **kw):
    args = (sequence,)
    for k in kw:
        args = args + (k,)

    reveal_type(args)  # revealed: tuple[sequence@f | str, ...]
```

## an `assert` whose narrowing takes away the reason it narrowed

what a body requires of an unannotated parameter is read off the body, and the body is then checked
against what that reading produced, so the two are settled by running them against each other until
they agree.

`assert isinstance(proto, int) and proto <= 5` says `proto` has to be an `int`. once that is
`proto`'s bound, though, `isinstance(proto, int)` is statically true — and an arm of an `and` that
is always true says nothing about which branch this is, so it is dropped and its narrowing goes with
it. the round after that has nothing to say about `proto`, which puts the bound back where it
started and lets the round after that find the narrowing again. neither round repeats the one before
it.

what the body requires is a fact about the body, so a requirement one round found is not taken away
by a round that cannot find it.

the signature is read from inside another function: reading it from the module asks for it before
anything has been inferred, which is not the order that reaches the cycle at all.

```py
def opcode(stack, proto):
    len(stack)
    assert isinstance(proto, int) and proto <= 5

def check():
    reveal_type(opcode)  # revealed: def opcode(stack: some Sized, proto: some int)
```

## a float literal a loop recomputes from itself

basedpython folds arithmetic on float literals, so `t` is a different literal on every pass of the
loop: `0.1`, then `0.2`, then `0.4`. The type of a name bound in a loop is the union of every value
that reaches it, and each round of the fixed-point iteration adds the value the round before it
produced, so no round ever repeats the one before it.

The union builder already stands a group of literals down to their instance type once a union
defined in terms of itself holds more of them than the fixed point can afford — which is how a loop
counting `int` literals settles on `int`. Float and complex literals have no such group, so nothing
bounded them.

```by
def run():
    t = 0.1
    while True:
        t = t * 2
        reveal_type(t)  # revealed: float
```

## a float literal a loop recomputes from itself through a generic call

The same growth reaches the union a generic call's solve builds, which is assembled separately from
the one that approximates the loop. `min` hands back whatever its arguments have in common, so the
literal the last pass produced comes back out of the call and goes round again.

```by
def run():
    t = 0.1
    while True:
        t = min(0.1, t * 2)
        reveal_type(t)  # revealed: float
```

## a float literal a recursive function recomputes from itself through a generic call

A loop is not the only thing that can recompute a value from itself. `rec` calls itself to get the
value it doubles, so its inferred return type grows the same way — but there is no loop header here,
and the only union that grows is the one the generic call's solve builds out of `min`'s arguments.

The solve collects one lower bound per element, so the union `rec` returned is taken apart before
the solve ever builds anything. Whatever recorded that the union was defined in terms of itself has
to survive being taken apart, or the union the solve builds back up has no reason to give its
literals up and grows one element per round with no fixed point.

```by
def rec():
    return min(0.1, rec() * 2)

def check():
    reveal_type(rec())  # revealed: float
```

## a complex literal a loop recomputes from itself

Complex literals fold the same way and are held the same way, so they need the same bound.

```by
def run():
    t = 1j
    while True:
        t = t * 2
        reveal_type(t)  # revealed: complex
```

## float literals a loop does not recompute

Nothing is given up when the values a loop binds do not depend on the ones it bound before, however
many of them there are.

```by
def run(flag: int):
    for _ in range(3):
        if flag == 0:
            t = 0.1
        elif flag == 1:
            t = 0.2
        elif flag == 2:
            t = 0.3
        elif flag == 3:
            t = 0.4
        elif flag == 4:
            t = 0.5
        else:
            t = 0.6
        reveal_type(t)  # revealed: 0.1 | 0.2 | 0.3 | 0.4 | 0.5 | 0.6
```

## a first-party module that shadows the home of a known class

The `collections.abc` ABCs really live in `_collections_abc`, so a project carrying a file of that
name is what `Sequence` resolves to — and `str` is declared in terms of `Sequence` in the typeshed.
Working out what an instance of a known class is therefore runs through the shadowing file, whose
own classes ask for that known class back before it has one.

The metaclass here is load-bearing: a metaclass that is not itself a class is the thing that sends
working out `Base`'s metaclass through `str`, which is where the two directions meet.

`_collections_abc.py`:

```py
def meta(name, bases, namespace): ...

class Base(metaclass=meta): ...
class Sequence(Base): ...
```

Neither direction can finish before the other, and while that is being untangled the known class
answers `Unknown` — the same thing it answers when it cannot be found at all. Once the recursion has
settled it is the class the typeshed declares again, so nothing outside the cycle pays for it:

```py
reveal_type("a".upper())  # revealed: LiteralString
reveal_type([1, 2, 3])  # revealed: list[int]
```

## the class a literal falls back to, while the module it lives in is still being untangled

A project can carry files named after several stdlib modules that import one another, and then its
own `typing`, `warnings` and `linecache` are in a cycle with each other. `typing` is where the
typeshed reaches for the pieces `builtins` is written in terms of, so while that cycle is being
untangled, *which class `str` is* has no answer yet either.

`linecache.py`:

```py
def getline(lineno):
    if lineno:
        return lines[lineno - 1]  # error: [unresolved-reference]
    return ""
```

`warnings.py`:

```py
import linecache

line = linecache.getline(1)

def _deprecated(*, remove, _version=sys.version_info):  # error: [unresolved-reference]
    pass
```

`typing.py`:

```py
import collections
import warnings

warnings._deprecated(remove=1)

Sequence = collections.abc.Sequence
```

`getline` says nothing about what it returns, so its return type is recovered from the cycle, and
recovering it rebuilds the union of everything the body hands back. Adding `Literal[""]` to a union
that already holds a `str` means deciding whether the two say the same thing — and asking that by
building the `str` this program means would re-enter, from inside the recovery, the very cycle being
recovered from. The class the existing type is already carrying answers it without going anywhere:

```py
reveal_type("a".upper())  # revealed: LiteralString
reveal_type([1, 2, 3])  # revealed: list[int]
```

## Known class instances with a shadowed typing module

String members retain their types when a local `typing.py` introduces an inference cycle. Resolving
`str`'s bases looks up `Sequence` in that module. Determining whether the assignment is reachable
requires inferring `trigger`'s return annotation. Resolving `C.attribute` requires determining `C`'s
metaclass. Checking a call to that unknown metaclass constructs a class namespace with `str` keys,
completing the cycle. The consumer is checked before the shadowing module.

This is a regression test for <https://github.com/astral-sh/ty/issues/4456>.

`m.py`:

```py
reveal_type("a".encode())  # revealed: bytes
```

`typing.py`:

```py
class C(metaclass=missing): ...  # error: [unresolved-reference]

def trigger() -> C.attribute: ...

trigger()
Sequence = object
```

## Known class instances after checking the shadowing module

String members retain their types when the shadowing module is checked first, as they do when the
consumer is checked first.

`typing.py`:

```py
class C(metaclass=missing): ...  # error: [unresolved-reference]

def trigger() -> C.attribute: ...

trigger()
Sequence = object
```

`m.py`:

```py
reveal_type("a".encode())  # revealed: bytes
```
