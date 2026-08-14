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

### inferred from a `float(...)` call

`float("inf")` is python's spelling of the value, so it infers the literal type the `float.inf`
annotation spells:

```by
a = float("inf")
reveal_type(a)  # revealed: inf

b: float.inf = float("inf")
c: -float.inf = float("-inf")
d: float.nan = float("nan")
```

### other literal arguments

any literal argument the constructor can convert folds to the float it constructs:

```by
reveal_type(float())  # revealed: 0.0
reveal_type(float("1.5"))  # revealed: 1.5
reveal_type(float("+Infinity"))  # revealed: inf
reveal_type(float(2))  # revealed: 2.0
reveal_type(float(True))  # revealed: 1.0
reveal_type(float(1.5))  # revealed: 1.5
```

### non-literal arguments

a call the checker cannot evaluate constructs an ordinary `float` — spelled
[`final float`](../basedpython_type_modifiers.md), as every constructor call is:

```by
def f(x: str, y: list[str]) -> None:
    reveal_type(float(x))  # revealed: final float
    # python accepts underscores and surrounding whitespace, which we don't parse
    reveal_type(float("1_000.5"))  # revealed: final float
    # error: [refutable-unpacking]
    reveal_type(float(*y))  # revealed: final float
```

### python files are unaffected

float literal types only exist in basedpython, so a python file infers a plain `float`:

```py
reveal_type(float("inf"))  # revealed: float
```
