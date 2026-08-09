# attribute types

a type parameter's members can be named in a type position. `T.a` is the type of
the member `a` on whatever `T` turns out to be:

```by
class A1:
    a: int

class A2(A1):
    a: bool

class B[T: A1]:
    x: T.a

def check(b1: B[A1], b2: B[A2]):
    reveal_type(b1.x)   # int
    reveal_type(b2.x)   # bool
```

`T` is unknown where the annotation is written, so the lookup cannot be
performed there. it is instead kept symbolic — like an
[arithmetic operation on a type parameter](symbolic-type-ops.md) — and
re-resolved against each specialization, so a subclass that redeclares the
member is honoured rather than flattened to the bound's declaration.

the same works for a function's type parameters:

```by
def get[T: A1](t: T) -> T.a:
    return t.a

reveal_type(get(A1()))  # int
reveal_type(get(A2()))  # bool
```

## a specialized receiver

the receiver does not have to be a bare type parameter. any type expression works,
so a generic can be asked what a member of it becomes at a given specialization:

```by
class X[T: A1]:
    x: T
    y: T.a

class Z:
    p: X[A1].x      # A1
    q: X[A2].y      # bool
    r: X[A2].x.a    # bool — receivers chain
```

a receiver that still mentions a type parameter stays symbolic, so it re-resolves
at each specialization just as the bare form does:

```by
class W[T: A1]:
    w: X[T].y       # `int` for `W[A1]`, `bool` for `W[A2]`
```

## scope

a *dotted name* is the one receiver shape that keeps its ordinary meaning: it
names the type it resolves to, so `mod.Class` and `Outer.Inner` are unaffected
and only a type parameter is read as an attribute type there.

```by
class Outer:
    class Inner: ...

x: Outer.Inner      # an `Inner` instance, not the type of the member `Inner`
```

every other receiver shape — a subscript, a chain built on one — has no other
meaning in a type position, so there is nothing to collide with.

a parameter pack is not a receiver either: `P.args` and `P.kwargs` name the
components of the pack rather than a member of it. nor is an `Annotated`
metadata element a type position — `Annotated[int, T.a]` is an attribute access
on the type parameter itself, not an attribute type.

the receiver may be a class or a function type parameter, and the attribute type
may appear anywhere a type may: an annotation, a signature, inside another type,
or as the value of a generic type alias.

```by
type Alias[T: A1] = T.a
```

any member works — a field, a method, a property, a nested class, or one
inherited from a base of the bound. a member the bound does not have is reported
as an unresolved attribute, so an attribute type is checked where it is written
and not only where it is specialized:

```by
class B[T: A1]:
    x: T.nope       # error: `T@B` has no attribute `nope`
```

`T: (A1, A2)` is a *tuple* bound, not a constraint list — `T` is bounded by the
tuple type, which has no `a`, so `T.a` there is an unresolved attribute:

```by
class B[T: (A1, A2)]:
    x: T.a      # error: `T@B` has no attribute `a`
```

a parameter constrained by a [type mapping](type-mappings.md) does resolve, per
constraint, because each specialization picks one of them:

```by
class C[T in (A1, A2)]:
    y: T.a      # `int` for `C[A1]`, `bool` for `C[A2]`
```

in a value position the same access unions over the constraints, since the
parameter stands for any one of them: `t.a` over `T in (A1, A2)` is
`int | bool`. the lowered annotation is that union too, which is the widening
every attribute type makes when python cannot spell the dependency.

before it is specialized, an attribute type behaves as the member's type on the
parameter's bound — the guarantee every specialization satisfies:

```by
def f[T: A1](b: B[T]):
    reveal_type(b.x)    # int
```

## polyfill

there is no runtime construct. python cannot express a member type that depends
on a type parameter, so the annotation is resolved at transpile time and written
out as the member's type on the parameter's bound:

```by
class B[T: A1]:
    x: T.a
    xs: list[T.a]

type Alias[T: A1] = T.a
```

transpiles to:

```python
class B[T: A1]:
    x: int
    xs: list[int]

type Alias[T: A1] = int
```

a member whose type has no python spelling at all — a method, whose type is a
bound method — is written out as `Any`. the precise type is still enforced
inside the `.by` file; only the emitted annotation widens. this is the same
trade every [symbolic operation](symbolic-type-ops.md) makes when it lowers to
its reduced form, and it is not reported: the source is valid, and only the
runtime artifact is less precise than the `.by` file it came from.
