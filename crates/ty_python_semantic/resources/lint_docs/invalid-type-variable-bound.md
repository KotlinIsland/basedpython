## What it does

Checks for [type variables][type variable] whose bounds reference type variables that are not in
scope where the bound is written.

## Why is this bad?

A type parameter's bound may reference a type parameter that is already in scope *and* substituted
where the bound is used: an earlier entry in the same type parameter list, or — for a method — an
entry in its class's list, which binding the receiver settles.

Anything else has nothing to resolve to. A later entry is not yet in scope, a parameter is not in
scope inside its own bound, and a legacy `TypeVar` is declared by an assignment, so it has no list
to hold a position in. A nested class or nested function is in scope but is never substituted, so
the reference would still be standing at every use of the generic. A variadic pack's bound describes
its members rather than the pack's own value, so it cannot reference a type parameter at all.

## Examples

```toml
[environment]
python-version = "3.12"
```

```python
from typing import TypeVar

# error: [invalid-type-variable-bound]
RecursiveT = TypeVar("RecursiveT", bound=list["RecursiveT"])
U = TypeVar("U")
# error: [invalid-type-variable-bound]
BoundT = TypeVar("BoundT", bound=U)


def f[T: list[T]](): ...  # error: [invalid-type-variable-bound]
def g[T: U, U](): ...  # error: [invalid-type-variable-bound]


# `U` precedes `T`, so `T`'s bound can name it
def h[U, T: U](x: U, y: T): ...


class Owner[U]:
    # the receiver settles `U`, so a method's bound can name it
    def narrow[T: U](self, x: T) -> U:
        return x

    # nothing settles `U` for a nested class
    class Inner[T: U]: ...  # error: [invalid-type-variable-bound]
```

[type variable]: https://docs.python.org/3/library/typing.html#typing.TypeVar
