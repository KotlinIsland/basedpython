# basedpython: `is` / `is not` keyword narrowing

In basedpython, the `is` and `is not` keyword pair perform instance checks (they transpile to
`isinstance(...)` / `not isinstance(...)`). The `===` and `!==` operators retain Python's identity
comparison semantics. Narrowing in `.by` files mirrors this swap.

## `is not` narrows to negation of the instance type

```by
def f(a: object):
    if a is not int:
        reveal_type(a)  # revealed: not int
```

## `is` narrows to the instance type

```by
def f(a: object):
    if a is int:
        reveal_type(a)  # revealed: int
```

## `!==` keeps Python identity semantics

```by
def f(a: object):
    if a !== None:
        reveal_type(a)  # revealed: not None
```

## `===` keeps Python identity semantics

```by
def f(a: int | None):
    if a === None:
        reveal_type(a)  # revealed: None
```

## `is` with literal RHS keeps Python identity semantics

`isinstance(x, None)` is invalid at runtime, so `is`/`is not` against literal singletons (`None`,
`True`/`False`, numbers, strings, bytes, `...`) must transpile as Python `is`/`is not` rather than
`isinstance`.

```by
def f(a: int | None):
    if a is None:
        reveal_type(a)  # revealed: None
    if a is not None:
        reveal_type(a)  # revealed: int
```

```by
def f(a: bool | int):
    if a is True:
        reveal_type(a)  # revealed: True
    if a is False:
        reveal_type(a)  # revealed: False
```

```by
def f(a: int | None):
    if a is ...:
        reveal_type(a)  # revealed: Never
```

## `is` with an enum member RHS keeps Python identity semantics

An enum member is a singleton *instance*, not a class — `isinstance(x, Color.RED)` would be a
runtime `TypeError` — so `is`/`is not` against a member keeps Python identity semantics and narrows
by identity, the same as literal singletons.

```by
import enum

class Color(enum.Enum):
    RED = 1
    GREEN = 2

def f(c: Color):
    if c is Color.RED:
        reveal_type(c)  # revealed: Color.RED
    if c is not Color.RED:
        reveal_type(c)  # revealed: Literal[Color.GREEN]
```

The same holds for based-enum members:

```by
enum class Genre:
    case A, B

def g(x: Genre):
    if x is Genre.A:
        reveal_type(x)  # revealed: Genre.A
    if x is not Genre.A:
        reveal_type(x)  # revealed: Literal[Genre.B]
```

## An instance check yields `bool`, never an identity fold

The keyword form is an instance check, so Python's identity folds (an instance is never identical to
a class object, so plain Python would type `x is int` as `Literal[False]`) must not apply —
otherwise everything after `assert x is int` would be unreachable.

```by
def f(x: object):
    b = x is int
    reveal_type(b)  # revealed: bool
    assert x is int
    reveal_type(x)  # revealed: int
```
