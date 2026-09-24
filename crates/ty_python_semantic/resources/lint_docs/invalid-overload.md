## What it does

Checks for various invalid `@overload` usages.

## Why is this bad?

The `@overload` decorator is used to define functions and methods that accepts different
combinations of arguments and return different types based on the arguments passed. This is mainly
beneficial for type checkers. But, if the `@overload` usage is invalid, the type checker may not be
able to provide correct type information.

## Examples

### Single overload

```py
from typing import overload


@overload
def foo(x: int) -> int: ...  # error
def foo(x: int | None) -> int | None:
    return x
```

### Missing implementation

```py
from typing import overload


@overload
def foo() -> None: ...  # error
@overload
def foo(x: int) -> int: ...
```

### Implementation default an overload's default doesn't describe

A call matching an overload that leaves an argument out is solved from the overload's default, but
the implementation runs with its own.

```py
from typing import TypeVar, overload

T = TypeVar("T")


@overload
def foo(x: T = 1) -> T: ...  # error
@overload
def foo(x: int, y: int) -> int: ...
def foo(x: object = "a", y: int = 0) -> object:
    return x
```

## References

- [Python documentation: `@overload`](https://docs.python.org/3/library/typing.html#typing.overload)
