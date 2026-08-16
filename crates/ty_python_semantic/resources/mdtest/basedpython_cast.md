# basedpython: `cast` keyword

In basedpython, `<value> cast <type>` reinterprets the value as the target type. It is purely static
— the transpiler lowers it to `typing.cast(<type>, <value>)`, with no runtime residue — so it is
only accepted where the checker can already prove the value is the target. A cast that is not such a
widening must say what happens when the claim turns out to be false: `cast!` raises, and `cast?`
yields `None`. Those two are covered in `basedpython_checked_cast.md`.

## simple cast

```by
a = 1
b = a cast object
reveal_type(b)  # revealed: object
```

## cast to union

An arm of the union the value already belongs to is enough.

```by
a = 1
b = a cast int | str
reveal_type(b)  # revealed: int | str
```

## a downcast is rejected

`cast` never looks at the value, so casting *down* — from a supertype to one of its subtypes — would
assert something nothing verifies.

```by
def f(a: object):
    # error: [unsound-cast] "Cast from `object` to `int` is not a widening"
    b = a cast int
    reveal_type(b)  # revealed: int
```

The fix rewrites the keyword to `cast!`, the form that keeps the target type.

```by
def g(a: object):
    b = a cast int  # snapshot
```

```snapshot
error[unsound-cast]: Cast from `object` to `int` is not a widening
 --> src/mdtest_snippet.by:6:9
  |
6 |     b = a cast int  # snapshot
  |         ^^^^^^^^^^
info: `cast` reinterprets the value without checking it
help: Use `cast!` to raise on a mismatch, or `cast?` to yield `None`
  |
5 | def g(a: object):
  -     b = a cast int  # snapshot
6 +     b = a cast! int  # snapshot
7 | def f(a: object):
  |
note: This is an unsafe fix and may change runtime behavior
```

The two suffixed forms are accepted in its place.

```by
def f(a: object):
    b = a cast! int
    reveal_type(b)  # revealed: int
    c = a cast? int
    reveal_type(c)  # revealed: int | None
```

A sidecast — between two types where neither is the other — is rejected for the same reason.

```by
class Left: ...
class Right: ...

def f(x: Left):
    # error: [unsound-cast] "Cast from `Left` to `Right` is not a widening"
    x cast Right
```

## a gradual value is not already the target

Nothing at all is known about an `Any` value, which is exactly when the runtime check is worth
having, so it gets no free pass either.

```by
from typing import Any

def f(a: Any):
    # error: [unsound-cast] "Cast from `Any` to `int` is not a widening"
    a cast int
```

## a cast to the value's own type

Casting a value to the type it already has is a widening in the degenerate case, so it stands.

```by
def f(a: int):
    b = a cast int
    reveal_type(b)  # revealed: int
```

## statement cast narrows in place

A bare `<value> cast <type>` statement narrows the value for the rest of the scope, like an
unconditional assertion. The target type replaces the operand's prior type wholesale.

```by
class Base: ...
class Sub(Base): ...

def f(a: Sub):
    reveal_type(a)  # revealed: Sub
    a cast Base
    reveal_type(a)  # revealed: Base
```

`cast!` narrows the same way — it raises rather than carry on with a value that is not the target.

```by
def f(a: object):
    a cast! int
    reveal_type(a)  # revealed: int
```

`cast?` can yield `None`, so it does not narrow its operand.

```by
def f(a: object):
    a cast? int
    reveal_type(a)  # revealed: object
```

## non-overlapping cast

A cast between disjoint types can never succeed — `cast!` always raises and `cast?` always returns
`None` — so it is flagged.

```by
def f(a: object):
    a cast! int  # ok — `object` overlaps `int`
    # error: [non-overlapping-cast] "Cast from `""` to `int` is between non-overlapping types"
    "" cast! int
    # error: [non-overlapping-cast] "Cast from `"s"` to `int` is between non-overlapping types"
    b = "s" cast? int
    reveal_type(b)  # revealed: int | None
```

A disjoint plain `cast` is already rejected as unsound, and only that sharper report fires — the two
would otherwise stack on the same expression.

```by
def f():
    # error: [unsound-cast] "Cast from `""` to `int` is not a widening"
    "" cast int
```

## cast in call argument

```by
def f(x: object) -> object:
    return x

a = 1
reveal_type(f(a cast object))  # revealed: object
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
