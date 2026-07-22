# basedpython: `cast` keyword

In basedpython, `<value> cast <type>` narrows the value to the target type. By default the
transpiler lowers it to a runtime-checked cast that raises on a type mismatch; with checked casts
disabled it degrades to `typing.cast(<type>, <value>)`. Either way its inferred type is the target
type, so the checker sees the same narrowing shown below.

## simple cast

```by
a = 1
b = a cast int
reveal_type(b)  # revealed: int
```

## cast to union

```by
a = 1
b = a cast int | str
reveal_type(b)  # revealed: int | str
```

## statement cast narrows in place

A bare `<value> cast <type>` statement narrows the value for the rest of the scope, like an
unconditional assertion. The target type replaces the operand's prior type wholesale.

```by
def f(a: object):
    reveal_type(a)  # revealed: object
    a cast int
    reveal_type(a)  # revealed: int
```

`cast?` can yield `None`, so it does not narrow its operand.

```by
def f(a: object):
    a cast? int
    reveal_type(a)  # revealed: object
```

## non-overlapping cast

A `cast` between disjoint types can never succeed — a checked `cast` always raises and a safe
`cast?` always returns `None` — so it is flagged.

```by
def f(a: object):
    a cast int  # ok — `object` overlaps `int`
    # error: [non-overlapping-cast] "Cast from `""` to `int` is between non-overlapping types"
    "" cast int
    # error: [non-overlapping-cast] "Cast from `"s"` to `int` is between non-overlapping types"
    b = "s" cast? int
    reveal_type(b)  # revealed: int | None
```

Overlapping casts stay silent, including a downcast to a subtype.

```by
class Base: ...
class Sub(Base): ...

def f(b: Base):
    b cast Sub
    reveal_type(b)  # revealed: Sub
```

## cast in call argument

```by
def f(x: int) -> int:
    return x

a = 1
reveal_type(f(a cast int))  # revealed: int
```

## erased type arguments

A checked cast validates its value with `isinstance`, which can only test a class. A builtin
container erases its type arguments at runtime, so only the origin is checkable and the argument
claim goes unverified.

```by
def f(a: object):
    # error: [erased-cast-argument] "Type arguments of `list[int]` are erased at runtime"
    b = a cast list[int]
    reveal_type(b)  # revealed: list[int]
```

`cast?` narrows to `<type> | None`, and warns the same way.

```by
def f(a: object):
    # error: [erased-cast-argument] "Type arguments of `dict[str, int]` are erased at runtime"
    b = a cast? dict[str, int]
    reveal_type(b)  # revealed: dict[str, int] | None
```

A bare target claims no arguments, so there is nothing to erase — only the unrelated
`missing-type-argument` fires.

```by
def f(a: object):
    # error: [missing-type-argument] "Missing type argument for generic class `list` (expected 1 type argument)"
    b = a cast list
    reveal_type(b)  # revealed: list[Unknown]
```

A *user* generic's instances carry `__orig_class__`, so its arguments are checked in full.

```by
class A[T]:
    def __init__(self, t: T): ...

def f(a: object):
    b = a cast A[int]
    reveal_type(b)  # revealed: A[int]
```

## a statically-proven upcast is not erased

When the value is already the target statically, the cast verifies nothing at runtime, so no
argument claim is dropped — `erased-cast-argument` must stay silent even for a builtin target.
`B[int]` subclasses `list[int]`, so the argument is already guaranteed.

```by
class B[T](list[T]): ...

def f():
    b = B[int]() cast list[int]  # no erased-cast-argument: already a `list[int]`
    reveal_type(b)  # revealed: list[int]
```

The same holds for a subscripted protocol target, whose runtime `isinstance` would otherwise be an
error. Since the argument is covariant `object`, an `A[int]` is already a `Sequence[object]`.

```by
from collections.abc import Sequence

class A[T](Sequence[T]):
    def __getitem__(self, i): ...
    def __len__(self): ...

def f():
    a = A[int]() cast Sequence[object]
    reveal_type(a)  # revealed: Sequence[object]
```

A dynamic value is *not* a subtype of a concrete target, so its check is kept and the argument is
still reported as erased.

```by
def f(a):
    # error: [erased-cast-argument] "Type arguments of `list[int]` are erased at runtime"
    b = a cast list[int]
    reveal_type(b)  # revealed: list[int]
```

## user generic arguments are checked, not assumed

`T` is invariant here, so an `A[str]` is not an `A[int]`. These assertions run for real: the
divergence harness executes every checker-clean block.

```by
class A[T]:
    t: T
    def __init__(self, t: T):
        self.t = t

def f(x: object) -> A[int] | None:
    return x cast? A[int]

assert f(A(1)) is not None
assert f(A("")) is None  # right base, wrong argument
assert f(1) is None  # wrong base
```

## a data-member protocol target is checked structurally

A protocol has no `__orig_class__` to probe, but basedpython reifies class attribute annotations, so
a protocol whose members are all data members is validated structurally against those annotations —
no `erased-cast-argument`, and the cast is checked in full.

```by
from typing import Protocol

class HasA[T](Protocol):
    a: T

def f(x: object):
    b = x cast HasA[int]  # no erased-cast-argument: checked structurally
    reveal_type(b)  # revealed: HasA[int]
```

These assertions run for real — `a` is invariant, so a `bool` annotation is not an `int`:

```by
from typing import Protocol

class HasA[T](Protocol):
    a: T

class IntAttr:
    a: int

class BoolAttr:
    a: bool

def f(x: object) -> HasA[int] | None:
    return x cast? HasA[int]

assert f(IntAttr()) is not None
assert f(BoolAttr()) is None  # right member, wrong annotation
```

## a method-bearing protocol target cannot be checked

A method member's specialization isn't recoverable from a reified annotation, so the whole cast has
no runtime residue. The transpiler degrades it to an unchecked `typing.cast`; ty warns that it is
unchecked.

```by
from typing import Protocol

class HasGet[T](Protocol):
    def get(self) -> T: ...

def f(x: object):
    # error: [erased-cast-argument] "`HasGet[int]` cannot be checked at runtime"
    b = x cast HasGet[int]
    reveal_type(b)  # revealed: HasGet[int]
```

## not valid in `.py` files

`cast` as an infix soft keyword is basedpython-only. A `.py` file using it gets a parse error from
the parser.

```py
a = 1
# error: [invalid-syntax] "`cast` keyword is not valid in .py files"
b = a cast int
```

## regular `cast` call still works

A bare `cast(...)` call is parsed as an ordinary function call in both `.by` and `.py` files.

```py
from typing import cast

a = 1
b = cast(int, a)
reveal_type(b)  # revealed: int
```
