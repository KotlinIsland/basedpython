# basedpython: tuples as callable parameters

In basedpython, parenthesized tuples are equivalent to callable parameter lists. The same surface
syntax — including positional-only `/`, keyword-only `*`, named fields, and variadic `*: T` /
`**: T` — describes both a tuple type and a callable's parameter spec.

```toml
[environment]
python-version = "3.12"
```

## simple tuple

A plain tuple type acts as a positional parameter list when used as the first argument to
`Callable`.

```by
from typing import Callable

a: (int, str)
b: Callable[(int, str), int]
```

## variadic tuple

`*: T` makes the tuple variadic — equivalent to `tuple[..., *tuple[T, ...]]` as a tuple type.

```by
b: (int, *: str)
```

## mixed positional/named tuple

`/` separates positional-only fields from named ones. A named field's `name:` is metadata for
callable usage and is dropped when the tuple is used as a tuple type.

```by
c: (int, /, name: str)
```

## variadic tuple assignment

A tuple value with arbitrarily many elements is assignable to a `(*: T)` annotation since the
variadic tuple type is `tuple[T, ...]`.

```by
a: (*: int) = (1, 2, 3, 4)
reveal_type(a)  # revealed: (1, 2, 3, 4)

# Bad element type — should error
b: (*: int) = (1, "x", 3)  # error: [invalid-assignment]
```

## prefix + variadic + suffix tuple

```by
a: (int, *: str, bool) = (1, "x", "y", True)
reveal_type(a)  # revealed: (1, "x", "y", True)
```

## named fields without markers act as anon-named-tuple

```by
v: (int, name: str) = (1, "y")
reveal_type(v)  # revealed: (int, name: str)
reveal_type(v.name)  # revealed: str
```

## Callable with tuple parameters

```by
from typing import Callable

def f1(g: Callable[(int, str), bool]) -> bool: return g(1, "x")
def f2(g: Callable[(*args: int), int]) -> int: return g(1, 2, 3)
def f3(g: Callable[(int, /, name: str), bool]) -> bool: return g(1, name="y")
def f4(g: Callable[(*args: int, **kwargs: str), None]) -> None: ...

reveal_type(f1)  # revealed: def f1(g: (int, str, /) -> bool) -> bool
reveal_type(f2)  # revealed: def f2(g: (*args: int) -> int) -> int
reveal_type(f3)  # revealed: def f3(g: (int, /, name: str) -> bool) -> bool
reveal_type(f4)  # revealed: def f4(g: (*args: int, **kwargs: str) -> None)
```
