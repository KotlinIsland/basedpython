# basedpython: `var` declarations

`var` is the mutable counterpart of `let`: it marks the declaration site of a variable and nothing
more. `var x = v` means exactly `x = v` and `var x: T = v` means exactly `x: T = v`, so — unlike
`let`, which declares `Final` (see `basedpython_final.md`) — the binding stays writable and an
untyped `var` puts no declared type on the name.

```toml
[environment]
python-version = "3.12"
```

## an untyped `var` infers its value's type

```by
var a = 1
reveal_type(a)  # revealed: 1
```

## an untyped `var` leaves the binding unconstrained

There is no declared type, so a later assignment of an unrelated type is as legal as it is for a
plain python assignment.

```by
var a = 1
a = "x"
reveal_type(a)  # revealed: "x"
```

## a typed `var` declares its type

```by
var b: int = 1
reveal_type(b)  # revealed: 1

b = 2
b = "nope"  # error: [invalid-assignment]
```

## a typed `var` rejects a bad initializer

```by
var bad: int = "nope"  # error: [invalid-assignment]
```

## a valueless `var` still declares

```by
var c: int

c = 1
reveal_type(c)  # revealed: 1

c = "nope"  # error: [invalid-assignment]
```

## `var` is not `final`

Contrast `let d = 1`, where the second assignment is an error.

```by
var d = 1
d = 2
```

## inside a class body

```by
class C:
    var tag: str = "c"
    var count = 0

reveal_type(C.tag)  # revealed: str
reveal_type(C.count)  # revealed: int
C.tag = "x"
C.tag = 1  # error: [invalid-assignment]
```

## inside a function body

```by
def f() -> int:
    var total: int = 0
    total = total + 1
    return total
```

## a modifier may precede `var`

```by
private var e: int = 1
e = 2
e = "nope"  # error: [invalid-assignment]
```

## an untyped `var` is not a declaration for importers either

A `var` that states no type must not record a declared `Unknown`, or every reader that prefers a
declaration over an inference — an importer, a class attribute lookup — would lose the type.

`m.by`:

```by
var exported = 1
```

```by
from m import exported

reveal_type(exported)  # revealed: 1
```

## `var` is still an ordinary identifier

The declaration forms are `var NAME = v` and `var NAME: T [= v]`; everything else is a plain name.

```by
var = 5
reveal_type(var)  # revealed: 5

print(var)
var, y = 1, 2
```
