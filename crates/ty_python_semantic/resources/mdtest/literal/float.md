# Float literals

## Basic

```py
reveal_type(1.0)  # revealed: float
```

## basedpython: infinity and not-a-number

`float.inf` / `float.nan` (and `-float.inf`) are the special float-literal types. python has no
literal syntax for them, so they only exist in basedpython source — the transpiler erases them to
plain `float`.

### the literal types

bound as parameters so the inferred type can be revealed:

```by
def f(pos: float.inf, neg: -float.inf, nan: float.nan) -> None:
    reveal_type(pos)  # revealed: inf
    reveal_type(neg)  # revealed: -inf
    reveal_type(nan)  # revealed: nan
```

### each is a subtype of `float`

a special float literal is assignable to `float`, but a plain `float` is not assignable back to the
literal:

```by
def f(inf: float.inf, x: float) -> None:
    a: float = inf
    # error: [invalid-assignment]
    b: float.inf = x
```

### infinities keep their sign

`float.inf` and `-float.inf` are distinct types:

```by
def f(pos: float.inf) -> None:
    # error: [invalid-assignment]
    neg: -float.inf = pos
```

### nan is signless

every nan literal is the same type, so `-float.nan` is just `float.nan`:

```by
def f(nan: float.nan) -> None:
    also_nan: -float.nan = nan
    reveal_type(also_nan)  # revealed: nan
```

### in a return annotation

```by
def f() -> float.inf:
    raise NotImplementedError

reveal_type(f())  # revealed: inf
```
