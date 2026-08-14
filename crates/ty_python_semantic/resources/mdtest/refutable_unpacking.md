# Refutable unpacking

An unpacking assignment binds all of its targets or none of them: if the value does not yield the
number of elements the targets ask for, it raises `ValueError` and nothing is bound. `basedpython`
reports an unpacking whose value is not known to have that number of elements.

## A variable-length tuple

`tuple[int, ...]` says nothing about how many elements there are, so two targets may be one too
many:

```py
def f() -> tuple[int, ...]:
    return ()

# error: [refutable-unpacking] "`tuple[int, ...]` may not have exactly 2 elements, which would raise `ValueError` when unpacked"
a, b = f()
```

## A variable-length tuple in a `.by` file

The check is not tied to a source language — a `.by` file reports the same unpacking:

```by
def f() -> tuple[int, ...]:
    return ()

a, b = f()  # error: [refutable-unpacking] "`(*: int)` may not have exactly 2 elements, which would raise `ValueError` when unpacked"
```

## A fixed-length tuple

A tuple whose length is part of its type is checked exactly, by the existing length diagnostics, and
is not reported here:

```py
def f() -> tuple[int, str]:
    return (1, "a")

a, b = f()
reveal_type(a)  # revealed: int
reveal_type(b)  # revealed: str
```

## Any iterable whose length is not in its type

A `list`, a `set` and a custom iterator are all as unknown in length as a variable-length tuple:

```py
class Iterator:
    def __next__(self) -> int:
        return 1

class Iterable:
    def __iter__(self) -> Iterator:
        return Iterator()

def f(numbers: list[int], names: set[str], values: Iterable):
    a, b = numbers  # error: [refutable-unpacking]
    c, d = names  # error: [refutable-unpacking]
    e, g = values  # error: [refutable-unpacking]
```

## A starred target

A starred target absorbs any number of elements, so it only requires the targets around it. Those
still have to be there: an empty list has nothing to give to `first`.

```py
def f(numbers: list[int]):
    # error: [refutable-unpacking] "`list[int]` may not have at least 1 element, which would raise `ValueError` when unpacked"
    first, *rest = numbers
    *rest2, last = numbers  # error: [refutable-unpacking]
```

Requiring nothing is always satisfied:

```py
def f(numbers: list[int]):
    (*everything,) = numbers
    reveal_type(everything)  # revealed: list[int]
```

## A narrowed length

Narrowing a variable-length tuple to a fixed length makes the unpacking safe:

```py
def f(values: tuple[int, ...]):
    if len(values) == 2:
        a, b = values
        reveal_type(a)  # revealed: int
        reveal_type(b)  # revealed: int
```

## A mixed tuple

A mixed tuple pins its prefix and suffix and leaves the middle open, so it satisfies a starred
target that asks for no more than the two ends:

```toml
[environment]
python-version = "3.11"
```

```py
def f(values: tuple[int, *tuple[str, ...], bool]):
    first, *middle, last = values
    reveal_type(first)  # revealed: int
    reveal_type(last)  # revealed: bool

    # error: [refutable-unpacking] "`tuple[int, *tuple[str, ...], bool]` may not have exactly 3 elements, which would raise `ValueError` when unpacked"
    a, b, c = values
```

## A `for` loop and a `with` statement

Every binder that unpacks is checked, not just an assignment:

```py
class Manager:
    def __enter__(self) -> list[int]:
        return [1, 2]

    def __exit__(self, *args: object) -> None: ...

def f(rows: list[list[int]]):
    for a, b in rows:  # error: [refutable-unpacking]
        pass

    with Manager() as (c, d):  # error: [refutable-unpacking]
        pass

    [(e, g) for e, g in rows]  # error: [refutable-unpacking]
```

## A splatted argument

`f(*values)` binds the parameters positionally out of `values`, so a length that does not match is a
`TypeError` for the same reason `a, b = values` is a `ValueError`:

```py
def f(a: int, b: int): ...
def g(values: tuple[int, ...]):
    # error: [refutable-unpacking] "`tuple[int, ...]` may not have exactly 2 elements, which would raise `TypeError` when unpacked into this call"
    f(*values)
```

## A splatted argument after a written one

The arguments written before the splat count towards the parameters, and a parameter with a default
does not have to be reached:

```py
def f(a: int, b: int, c: int = 0): ...
def g(values: tuple[int, ...]):
    # error: [refutable-unpacking] "`tuple[int, ...]` may not have between 1 and 2 elements, which would raise `TypeError` when unpacked into this call"
    f(1, *values)
```

## A splatted argument that nothing constrains

An `*args` parameter takes however many elements are left, so a splat only has to reach the
parameters before it:

```py
def takes_any(*args: int): ...
def takes_one_then_any(a: int, *args: int): ...
def f(values: tuple[int, ...]):
    takes_any(*values)  # ok — nothing is required and nothing is too much
    # error: [refutable-unpacking] "`tuple[int, ...]` may not have at least 1 element, which would raise `TypeError` when unpacked into this call"
    takes_one_then_any(*values)
    takes_one_then_any(1, *values)  # ok — `a` is already filled
```

## A forwarded `ParamSpec`

The forwarding idiom stays quiet for the same reason: a `ParamSpec` pairs an `*args` argument with
an `*args` parameter.

```toml
[environment]
python-version = "3.12"
```

```py
from typing import Callable

def deco[**P, R](func: Callable[P, R]) -> Callable[P, R]:
    def wrapper(*args: P.args, **kwargs: P.kwargs) -> R:
        return func(*args, **kwargs)

    return wrapper
```

## A gradual value

A value with no type has opted out of checking, so there is nothing to report:

```py
from typing import Any

def f(value: Any):
    a, b = value
```

An `Any` *element* type is a different statement: the length of a `list[Any]` is just as unknown as
the length of a `list[int]`.

```py
from typing import Any

def f(values: list[Any]):
    a, b = values  # error: [refutable-unpacking]
```

## An `Unknown` element type

`Unknown` is what ty fills in where the code said nothing — an unannotated `*args` parameter, a bare
`tuple` annotation. A length is not worth complaining about when the contents were never stated
either:

```py
class Base:
    def __init__(self, a: int) -> None: ...

class Child(Base):
    def __init__(self, *args, **kwargs) -> None:
        super().__init__(*args, **kwargs)

def f(pair: tuple):  # error: [missing-type-argument]
    a, b = pair
```

## An unannotated parameter

An unannotated parameter is bounded by what its function's body asks of it, and unpacking it is one
of the things being asked. Reporting that would be complaining about a requirement read off the line
doing the complaining:

```py
def f(key):
    row, col = key
```

A type variable someone wrote is reported, because its bound is a claim about every value the
parameter can take:

```toml
[environment]
python-version = "3.12"
```

```py
def g[T: tuple[int, ...]](values: T):
    a, b = values  # error: [refutable-unpacking]
```

## A union

Each member of a union is checked on its own, so a union of a fixed-length and a variable-length
tuple is reported once:

```py
def f(values: tuple[int, int] | tuple[str, ...]):
    # error: [refutable-unpacking] "`tuple[str, ...]` may not have exactly 2 elements, which would raise `ValueError` when unpacked"
    a, b = values
```

## A value that is not iterable

A value that cannot be unpacked at all is reported as such, and not also as a refutable unpacking:

```py
def f(value: int):
    a, b = value  # error: [not-iterable]
```

## Turning the check off

The check is on by default. Setting `refutable-unpacking` to `ignore` in `[tool.ty.rules]` turns it
off for the whole project:

```toml
[rules]
refutable-unpacking = "ignore"
```

```py
def f(values: tuple[int, ...]):
    a, b = values
```

## Suppressing a single unpacking

With the check left on, one site can opt out:

```py
def f(values: tuple[int, ...]):
    a, b = values  # ty: ignore[refutable-unpacking]
```
