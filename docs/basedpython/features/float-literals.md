# infinity and not-a-number float literals

python has no literal syntax for the special floating-point values, so they have
no literal *type* either — an annotation cannot say "this is infinity". basedpython
adds three special float-literal types, written as attributes of `float`:

- `float.inf` — positive infinity
- `-float.inf` — negative infinity
- `float.nan` — not-a-number

they exist only in **type positions**. the transpiler erases each to plain `float`
in the emitted python, since the runtime has no spelling for them:

```by
def clamp(lo: -float.inf, hi: float.inf) -> None:
    ...
```

transpiles to:

```python
def clamp(lo: float, hi: float) -> None:
    ...
```

## the literal types

bound as parameters so the inferred type can be revealed:

```by
def f(pos: float.inf, neg: -float.inf, nan: float.nan) -> None:
    reveal_type(pos)  # revealed: inf
    reveal_type(neg)  # revealed: -inf
    reveal_type(nan)  # revealed: nan
```

## each is a subtype of `float`

a special float literal is assignable to `float`, but a plain `float` is not
assignable back to the literal — same direction as any literal-to-base relation:

```by
def f(inf: float.inf, x: float) -> None:
    a: float = inf
    b: float.inf = x  # error: [invalid-assignment]
```

## infinities keep their sign

`float.inf` and `-float.inf` are distinct types — a positive infinity is not a
negative one:

```by
def f(pos: float.inf) -> None:
    neg: -float.inf = pos  # error: [invalid-assignment]
```

## nan is signless

a nan carries no sign, so `-float.nan` is the same type as `float.nan`:

```by
def f(nan: float.nan) -> None:
    also_nan: -float.nan = nan
    reveal_type(also_nan)  # revealed: nan
```

## type-position only

these are *types*, not values. python's runtime `float` has no `inf` / `nan`
attribute, so writing `float.inf` in a value position is rejected by the checker
and would fail at runtime — use the standard library (`math.inf`, `math.nan`, or
`float("inf")`) for the actual values:

```by
x: float = float.inf  # error: [unresolved-attribute]
```
