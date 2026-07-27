# basedpython: attribute types

```toml
[environment]
python-version = "3.13"
```

`T.a`, where `T` is a type parameter, is an *attribute type*: the type of the member `a` on whatever
`T` turns out to be. it cannot be resolved where it is written — `T` is unknown until the class or
function is specialized — so it is kept symbolic and re-resolved against each specialization, in the
same way an [arithmetic operation on a type parameter](basedpython_deferred_type_ops.md) is.

the receiver may be any type expression. a *dotted name* is the one shape that keeps its ordinary
meaning — `mod.Class` and `Outer.Inner` still name the type they resolve to — so only a type
parameter is read as an attribute type there.

## a member of a class type parameter

```by
class A1:
    a: int

class A2(A1):
    a: bool

class B[T: A1]:
    x: T.a

def check(b1: B[A1], b2: B[A2]):
    reveal_type(b1.x)  # revealed: int
    reveal_type(b2.x)  # revealed: bool
```

## a member of a function type parameter

```by
class A1:
    a: int = 0

class A2(A1):
    a: bool = True

def get[T: A1](t: T) -> T.a:
    return t.a

reveal_type(get(A1()))  # revealed: int
reveal_type(get(A2()))  # revealed: bool
```

## an attribute type in a parameter position

```by
class A1:
    a: int

class A2(A1):
    a: bool

def store[T: A1](t: T, value: T.a) -> None: ...

store(A2(), True)

# error: [invalid-argument-type] "Argument to function `store` is incorrect: Expected `bool`, found `1`"
store(A2(), 1)
```

## nested inside another type

```by
class A1:
    a: int

class A2(A1):
    a: bool

class B[T: A1]:
    xs: list[T.a]

def check(b1: B[A1], b2: B[A2]):
    reveal_type(b1.xs)  # revealed: list[int]
    reveal_type(b2.xs)  # revealed: list[bool]
```

## before specialization it is the bound's member

an unspecialized attribute type behaves as the member's type on the parameter's bound — that is the
guarantee every specialization satisfies.

```by
class A1:
    a: int

class B[T: A1]:
    x: T.a

def f[T: A1](b: B[T]):
    reveal_type(b.x)  # revealed: int
```

## methods and nested classes are members too

```by
class Inner:
    n: int

class A1:
    inner: Inner
    def describe(self) -> str:
        return ""

class B[T: A1]:
    d: T.describe
    m: T.inner

def check(b: B[A1]):
    reveal_type(b.d)  # revealed: bound method A1.describe() -> str
    reveal_type(b.m)  # revealed: Inner
```

## an inherited member

the lookup is an ordinary member lookup, so it reaches the bound's bases.

```by
class Base:
    inherited: str

class A1(Base):
    a: int

class B[T: A1]:
    x: T.inherited

def check(b: B[A1]):
    reveal_type(b.x)  # revealed: str
```

## in a generic type alias

```by
class A1:
    a: int

class A2(A1):
    a: bool

type Alias[T: A1] = T.a

def check(x: Alias[A1], y: Alias[A2]):
    reveal_type(x)  # revealed: int
    reveal_type(y)  # revealed: bool
```

## a specialized receiver

the receiver may be any type expression, not only a bare type parameter. `X[A].x` is the type of
`X`'s member `x` once `T` is `A` — the lookup runs against an instance of the receiver, just as it
does for the bare form.

```by
class A:
    a: int

class B(A):
    a: bool

class X[T: A]:
    x: T
    y: T.a

class Z:
    plain1: X[A].x
    plain2: X[B].x
    composed1: X[A].y
    composed2: X[B].y

def check(z: Z):
    reveal_type(z.plain1)  # revealed: A
    reveal_type(z.plain2)  # revealed: B
    reveal_type(z.composed1)  # revealed: int
    reveal_type(z.composed2)  # revealed: bool
```

## a specialized receiver composes and nests

```by
class A:
    a: int

class B(A):
    a: bool

class X[T: A]:
    x: T
    y: T.a

class Z:
    chained: X[B].x.a
    nested: list[X[B].y]

def check(z: Z):
    reveal_type(z.chained)  # revealed: bool
    reveal_type(z.nested)  # revealed: list[bool]
```

## a specialized receiver that is still symbolic

a receiver that mentions a type parameter keeps the whole attribute type symbolic, so it re-resolves
against each specialization exactly as the bare form does.

```by
class A:
    a: int

class B(A):
    a: bool

class X[T: A]:
    y: T.a

class W[T: A]:
    w: X[T].y

def check(wa: W[A], wb: W[B]):
    reveal_type(wa.w)  # revealed: int
    reveal_type(wb.w)  # revealed: bool
```

## a member the specialized receiver does not have

```by
class A:
    a: int

class X[T: A]:
    x: T

class Z:
    # error: [unresolved-attribute] "Object of type `X[A]` has no attribute `nope`"
    q: X[A].nope
```

## `Annotated` metadata is a value position, not a type

metadata is arbitrary runtime data, so the type-expression reading must not reach into it — `T.a`
there is an attribute access on the `TypeVar` object, exactly as it is outside an annotation.

```by
from typing import Annotated

class A1:
    a: int

class B[T: A1]:
    # error: [unresolved-attribute] "Object of type `TypeVar` has no attribute `a`"
    v: Annotated[int, T.a]

# error: [unresolved-attribute] "Object of type `TypeVar` has no attribute `a`"
def fn[T: A1](v: Annotated[int, T.a]) -> None: ...
```

## a constrained type parameter has no members

member lookup on a constrained type parameter does not union over the constraints — the same limit
applies to `t.a` in a value position — so an attribute type over one is an error rather than the
union of each constraint's member.

```by
class A1:
    a: int

class A2:
    a: bool

class B[T: (A1, A2)]:
    # error: [unresolved-attribute] "Object of type `T@B` has no attribute `a`"
    x: T.a
```

## no other dotted-name annotation changes meaning

for a dotted name the attribute-type reading is confined to a type-parameter receiver, so every
other dotted name in an annotation resolves exactly as it did before — including which diagnostic it
earns.

```by
type def F[X]:
    return int

# error: [invalid-type-form] "`F` is a `type def`; it can only be applied in a type expression, not used as a value"
x: F.nope
```

## a member the bound does not have is an error

```by
class A1:
    a: int

class B[T: A1]:
    # error: [unresolved-attribute] "Object of type `T@B` has no attribute `nope`"
    x: T.nope
```

## an unbounded type parameter only has `object`'s members

```by
class B[T]:
    # error: [unresolved-attribute] "Object of type `T@B` has no attribute `a`"
    x: T.a
```

## a parameter pack is not a receiver

`P.args` and `P.kwargs` name the components of a parameter pack, not a member of it.

```by
def f[**P](*args: P.args, **kwargs: P.kwargs) -> None: ...
```

## attribute types are `.by` only

```py
class A1:
    a: int

class B[T: A1]:
    # error: [unresolved-attribute] "Object of type `TypeVar` has no attribute `a`"
    x: T.a
```
