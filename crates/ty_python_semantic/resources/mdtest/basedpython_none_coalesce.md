# basedpython: `??` none-coalesce operator

`a ?? b` evaluates to `a` if `a is not None`, otherwise `b`. the result type is `T | U` where `T` is
the non-None portion of `a`'s type and `U` is `b`'s type.

```toml
[environment]
python-version = "3.12"
```

## simple coalesce with plain names

```by
def f(maybe: int | None, fallback: int) -> int:
    result = maybe ?? fallback
    reveal_type(result)  # revealed: int
    return result
```

## non-None literal short-circuits

```by
def f() -> int:
    reveal_type(5 ?? 10)  # revealed: 5 | 10
    return 5 ?? 10
```

## chained coalesce

```by
def f(a: int | None, b: int | None, c: int) -> int:
    result = a ?? b ?? c
    reveal_type(result)  # revealed: int
    return result
```

## the right operand is a branch

`a ?? b` evaluates `b` only when `a` is `None`, so `b` is not part of the flow that continues when
`a` is not `None`.

A binding made there is only possibly bound afterwards:

```by
def g() -> int:
    return 1

def f(a: int | None) -> int:
    result = a ?? (fallback := g())
    # error: [possibly-unresolved-reference]
    return result + fallback
```

And a diverging right operand does not end the enclosing flow:

```by
def f(a: int | None) -> int:
    result = a ?? raise ValueError()
    reveal_type(result)  # revealed: int
    return result
```

```by
def f(a: int | None) -> int:
    result = a ?? return 0
    reveal_type(result)  # revealed: int
    return result
```
