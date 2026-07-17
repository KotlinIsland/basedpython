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
