# basedpython: parametric type tests

`x is C[args]` (keyword form) tests a value against a *specialization*. The test is resolved
rust-style from static types wherever possible. When it can't be — the value's type is dynamic or a
mixed union — the last resort is a runtime probe that unwinds the value: its reified
`__orig_class__` (stamped on a user generic by `A[int](…)`) and every generic base declared across
`type(value).__mro__` (`__orig_bases__`). A concrete subclass that fixes its arguments
(`class B(list[int])`) is checkable this way, even against a builtin or abc target. A bare `list`
records nothing and answers `False`; the narrowing is positive-only, so that `False` is sound.

A protocol target has no `__orig_class__` to unwind, but basedpython reifies annotations, so a
protocol is checked structurally against them instead: a data member against the value class's
annotation, a method member against the value method's reified parameter (contravariant) and return
(covariant) annotations. Only a protocol with a member whose specialized type has no runtime
spelling (a callable attribute, a dynamic type) has no runtime residue and stays an error.

## statically decided tests are silent

```by
xs = [1, 2]
ys: list[object] = [1]

b1 = xs is list[int]
b2 = ys is list[int]
```

## reified type parameters carry the answer

The test on `x: T` lowers to an equality check of the reified `T` cell, so it is verified — and it
reifies `T` (the function transpiles with the `@generic` wrapper).

```by
def f[T](x: T) -> bool:
    return x is list[int]

def g[T](x: list[T]) -> bool:
    return x is list[int]
```

## a builtin union probes each value

A union of builtin specializations is probed, not rejected: the runtime unwinds each value's mro. A
bare `list` in either arm records no arguments and answers `False` — no element-witness guessing.

```by
def f(x: list[int] | list[str]) -> bool:
    return x is list[int]
```

## a concrete subclass fixes the arguments

A class that inherits from a specialization records it in `__orig_bases__`, so the probe recovers
the arguments accurately — no heuristic, no element inspection. `A` fixes `Sequence[int]` directly;
`B` fixes `list[int]`, and a `list[int]` is a `Sequence[int]`, so both hold at runtime.

```by
from collections.abc import Sequence

class A(Sequence[int]):
    def __getitem__(self, i): ...
    def __len__(self): ...

class B(list[int]): ...

def f(x: object) -> bool:
    return x is Sequence[int]

assert f(A())
assert f(B())
assert not f(object())
```

The same subclass is checkable against the builtin origin it fixes, while a bare `list` — which
records no arguments — answers `False`.

```by
class B(list[int]): ...

def g(x: object) -> bool:
    return x is list[int]

assert g(B())
assert not g([1, 2])
```

## variance is respected

`a is C[args]` means `type(a) <: C[args]`, so it follows `C`'s declared variance. With a covariant
`out T`, `A[int]` is an `A[object]` (because `int <: object`), and a statically-`A[int]` value folds
the test to `True`.

```by
class A[out T]:
    def __init__(self): ...

def f(a: A[int]) -> bool:
    return a is A[object]

def g(a: A[object]) -> bool:
    return a is A[int]
```

## use-site variance is respected

A target may spell its own variance (`A[out int]`), which projects an invariant `T` covariantly for
this one test — so `A[bool]` is an `A[out int]`, exactly as it is assignable to one. The projection
applies to the runtime probe too, not just the static fold.

```by
class A[in out T]:
    def __init__(self): ...

def f(a: A[bool]) -> bool:
    b: A[out int] = a  # assignable, so the test below is `True`
    return a is A[out int]

def g(a: A[*]) -> bool:
    return a is A[out int]

def h(a: A[*]) -> bool:
    return a is A[in int]

# without a projection `T` stays invariant, so a wider value is not a match
def i(a: A[bool]) -> bool:
    return a is A[int]
```

Narrowing keeps the projection rather than discarding it to `Unknown`:

```by
class B[in out T]:
    def __init__(self): ...

def f(b: B[*]):
    if b is B[out int]:
        reveal_type(b)  # revealed: B[*] & B[out int]
```

A projection cannot widen a declared-variant parameter beyond what it already allows — the declared
variance wins, so these behave exactly as the unprojected forms do:

```by
class C[out T]:
    def __init__(self): ...

def f(c: C[int]) -> bool:
    return c is C[out object]

def g(c: C[*]) -> bool:
    return c is C[out int]
```

## a user-generic union is discriminated by the probe

`A`'s instances carry `__orig_class__`, so each arm is distinguishable at runtime (an invariant
field keeps ty from collapsing the union).

```by
class A[T]:
    def __init__(self, t: T):
        self.v: list[T] = [t]

def f(x: A[int] | A[str]) -> bool:
    return x is A[int]
```

## a builtin target on a dynamic value probes

A dynamic value against a builtin target probes: a subclass that fixes the arguments matches, a bare
`list` answers `False`.

```by
def f(x) -> bool:
    return x is list[int]
```

## a builtin target through a local probes

The value reaches the test through a local `list` literal. It probes just the same — a bare list
records no arguments, so it answers `False`.

```by
def f[T](x: T) -> bool:
    y = [x]
    return y is list[int]
```

## a wide static type against a builtin probes

```by
def f(x: object) -> bool:
    return x is list[int]
```

## a user-defined generic target is valid

`A`'s instances carry `__orig_class__` (stamped by `A[int](…)`), so the runtime probe is a
legitimate check — no diagnostic.

```by
class A[T]:
    def __init__(self, t: T): ...

def f(x) -> bool:
    return x is A[int]

def g(x: object) -> bool:
    return x is A[int]
```

## a method-bearing protocol target is checked structurally

A protocol's instances record their own concrete class in `__orig_class__`, never the protocol, so a
probe could never match. But basedpython reifies method annotations too, so a *method* member is
checked against the value method's reified parameters (contravariant) and return (covariant) — no
error, and the test is a plain `bool`.

```by
from typing import Protocol

class P[T](Protocol):
    def get(self) -> T: ...

def f(x: object) -> bool:
    reveal_type(x is P[int])  # revealed: bool
    return x is P[int]
```

## a data-member protocol target is checked structurally

A protocol whose members are all data members *can* be checked: basedpython reifies class attribute
annotations, so the value's class is inspected member by member at runtime — each reified annotation
against the member's specialized type. No error, and the test is a plain `bool`.

```by
from typing import Protocol

class HasA[T](Protocol):
    a: T

def f(x: object) -> bool:
    reveal_type(x is HasA[int])  # revealed: bool
    return x is HasA[bool]
```

A concrete value is still decided statically — the structural runtime check is only the residue for
an undecidable value.

```by
from typing import Protocol

class HasA[T](Protocol):
    a: T

class C:
    a: bool

def f(c: C) -> bool:
    reveal_type(c is HasA[bool])  # revealed: bool
    return c is HasA[bool]
```

## a protocol mixing data and method members is checked

A protocol with both data and method members is checked member by member — each contributes its own
structural check.

```by
from typing import Protocol

class Mixed[T](Protocol):
    a: T
    def get(self) -> T: ...

def f(x: object) -> bool:
    reveal_type(x is Mixed[int])  # revealed: bool
    return x is Mixed[int]
```

Only a member whose specialized type has no runtime spelling makes the whole protocol uncheckable: a
callable attribute erases to nothing a reified annotation could pin down, so the test stays an
error.

```by
from typing import Protocol
from collections.abc import Callable

class WithCallable[T](Protocol):
    a: T
    cb: Callable[[T], T]

def f(x: object) -> bool:
    return x is WithCallable[int]  # error: [erased-type-check]
```

## a data-member protocol arm of a union is checked structurally

A data-member protocol is a legitimate arm of a union target — it contributes its structural check,
not an error.

```by
from typing import Protocol

class HasA[T](Protocol):
    a: T

def f(a: object) -> bool:
    return a is HasA[int] | int
```

## a decidable protocol target folds silently

Only the runtime-probe case errors; a statically provable protocol target folds like any other.

```by
from typing import Protocol

class P[T](Protocol):
    def get(self) -> T: ...

class C:
    def get(self) -> int:
        return 1

def f(c: C) -> bool:
    return c is P[int]
```

## a tuple target compares each element cell

`x: tuple[T, U]` carries both parameters in reified cells, so the test unifies position by position
against the tuple target.

```by
def f[T, U](x: tuple[T, U]) -> bool:
    return x is tuple[int, str]
```

## a nested generic value unifies structurally

`x: A[list[T]]` reaches `T` through two levels; the test descends the target structure to the cell.

```by
class A[T]:
    def __init__(self): ...

def f[T](x: A[list[T]]) -> bool:
    return x is A[list[int]]
```

## a multi-parameter target probes each argument

`Pair`'s two invariant parameters each get matched by the probe.

```by
class Pair[K, V]:
    def __init__(self, k: K, v: V):
        self.k: K = k
        self.v: V = v

def f(x: object) -> bool:
    return x is Pair[int, str]
```

## a bivariant parameter matches any specialization

A type parameter never used in the class body is bivariant, so every specialization is mutually
assignable and the probe matches either way.

```by
class Box[T]:
    def __init__(self): ...

def f(x: object) -> bool:
    return x is Box[int]
```

## a declared-contravariant target follows its variance

With `in T`, `Sink[int]` is a `Sink[bool]` (because `bool <: int`), so a statically-`Sink[int]`
value folds the test to `True`.

```by
class Sink[in T]:
    def __init__(self): ...
    def put(self, x: T) -> None: ...

def f(s: Sink[int]) -> bool:
    return s is Sink[bool]

def g(s: Sink[bool]) -> bool:
    return s is Sink[int]
```

## a decidable builtin target does not error

The erased-target error only fires when the test would fall back to a runtime probe. A value whose
static type settles the test — here a union disjoint from the target — folds silently even against a
builtin.

```by
def f(x: int | str) -> bool:
    return x is list[int]
```

## an alias name targets its specialization

A name bound to a specialization (`X = A[int]`) is that specialization, so `y is X` resolves exactly
as `y is A[int]` — a probe of a user generic here, no diagnostic.

```by
class A[T]:
    def __init__(self, t: T):
        self.v: list[T] = [t]

X = A[int]

def f(y: object) -> bool:
    return y is X
```

## an alias to a builtin specialization probes

Just like the spelled-out form.

```by
X = list[int]

def f(y: object) -> bool:
    return y is X
```

## a PEP 695 type alias targets its value

`type X = A[int]` resolves to `A[int]` the same way; the runtime probe unwraps the alias.

```by
class A[T]:
    def __init__(self, t: T):
        self.v: list[T] = [t]

type X = A[int]

def f(y: object) -> bool:
    return y is X
```

## a PEP 695 alias to a builtin probes

```by
type X = list[int]

def f(y: object) -> bool:
    return y is X
```

## a bare class name is an ordinary instance test

A name that is not a specialization keeps plain `isinstance` semantics — never an erased-target
error.

```by
def f(y: object) -> bool:
    return y is int
```

## a union target tests each arm

`a is T1 | T2` matches when `a` matches any arm, so it lowers to the disjunction of the per-arm
tests — each arm resolved by its own kind (a bare class by `isinstance`, a specialization by a probe
or fold). It never builds a runtime `isinstance(a, T1 | T2)`, which would fail on a parameterized
arm.

```by
class A[T]:
    def __init__(self, t: T):
        self.v: list[T] = [t]

def f(a: object) -> bool:
    return a is A[int] | object

def g(a: object) -> bool:
    return a is int | str | bytes
```

## a `None` arm is an identity check

`X | None` — an optional — tests the `None` arm by identity, not `isinstance(_, None)` (`None` is a
value, not a class).

```by
def f(a: object) -> bool:
    return a is int | None
```

## a builtin arm of a union probes

A builtin-specialization arm probes just like a standalone builtin target; the arm is
`_parametric_is` in the disjunction, next to the bare-class arm's `isinstance`.

```by
def f(a: object) -> bool:
    return a is list[int] | int
```

## an uncheckable protocol arm of a union is still rejected

A protocol arm with no runtime residue — a member whose specialized type has no runtime spelling —
remains an error inside a disjunction; it may not silently fold to `False`.

```by
from typing import Protocol
from collections.abc import Callable

class P[T](Protocol):
    cb: Callable[[T], T]

def f(a: object) -> bool:
    return a is P[int] | int  # error: [erased-type-check]
```

A *checkable* protocol arm, by contrast, contributes its structural check rather than an error.

```by
from typing import Protocol

class Q[T](Protocol):
    def get(self) -> T: ...

def f(a: object) -> bool:
    return a is Q[int] | int
```

## positive narrowing

The positive branch narrows to the tested specialization. The negative branch does not narrow: an
unreified value answers `False` even when its static type matches, so the test does not prove the
negation.

```by
class A[T]:
    def __init__(self, t: T):
        self.v: list[T] = [t]

def f(x: A[int] | A[str]):
    if x is A[int]:
        reveal_type(x)  # revealed: A[int]
    else:
        reveal_type(x)  # revealed: A[int] | A[str]
```

## `===` keeps identity semantics

```by
xs = [1]

b = xs === list[int]
```
