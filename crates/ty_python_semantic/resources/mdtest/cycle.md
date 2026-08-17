# Cycles

## Function signature

Deferred annotations can result in cycles in resolving a function signature:

```py
from __future__ import annotations

# error: [invalid-type-form]
def f(x: f):
    pass

reveal_type(f)  # revealed: def f(x: Unknown)
```

## Unpacking

See: <https://github.com/astral-sh/ty/issues/364>

```py
class Point:
    def __init__(self, x: int = 0, y: int = 0) -> None:
        self.x = x
        self.y = y

    def replace_with(self, other: "Point") -> None:
        self.x, self.y = other.x, other.y

p = Point()
reveal_type(p.x)  # revealed: int
reveal_type(p.y)  # revealed: int
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

## Self-referential implicit attributes

```py
class Cyclic:
    def __init__(self, data: str | dict):  # error: [missing-type-argument]
        self.data = data

    def update(self):
        if isinstance(self.data, str):
            self.data = {"url": self.data}

# revealed: str | dict[Unknown, Unknown] | dict[str, str]
reveal_type(Cyclic("").data)
```

## Cycle normalization preserves non-gradual variadic parameters

Normalizing a recursive implicit-attribute type does not reinterpret specialized variadic parameters
as gradual:

```py
from typing import Any, Callable, Generic, TypeVar
from ty_extensions import static_assert
from ty_extensions._internal import TypeOf, is_subtype_of

T = TypeVar("T")
flag: bool

class C(Generic[T]):
    def method(self, *args: T, **kwargs: T) -> None: ...

c = C[Any]()

class Recursive:
    def __init__(self, other: "Recursive"):
        self.callback = c.method if flag else other.callback

def check(value: Recursive):
    reveal_type(value.callback)  # revealed: bound method C[Any].method(*args: Any, **kwargs: Any)
    static_assert(is_subtype_of(TypeOf[value.callback], Callable[[], None]))
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

## Lazy cached property behind `hasattr`

This pattern used to panic with "too many cycle iterations".

```py
class Cached:
    def get(self) -> int:
        return 0

    @property
    def metadata(self) -> int:
        if not hasattr(self, "_metadata"):
            self._metadata = self.get()
        return self._metadata

reveal_type(Cached().metadata)  # revealed: int
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
