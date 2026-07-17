# cast

basedpython adds a `cast` infix soft keyword for inline type casts:

```by
b = a cast int
```

by default this is a **checked** cast — it verifies the value at runtime and
raises on a mismatch — so it transpiles to:

```python
def _checked_cast(_v, _t):
    if not isinstance(_v, _t):
        raise TypeError(
            f"cast to {getattr(_t, '__name__', _t)} failed: value is {type(_v).__name__}"
        )
    return _v

b = _checked_cast(a, int)
```

see [checked cast](checked-cast.md) for the runtime semantics, the safe `cast?`
variant, and the `--no-checked-cast` flag (which degrades `cast` to an
unchecked `typing.cast`).

## syntax

`cast` is an infix soft keyword: `<value> cast <type>`. the left operand is
the value being cast, the right operand is the target type

```by
b = a cast int | str             # checked cast to int | str
f(a cast int)                    # f(a checked-cast to int)
```

the inferred type is the target type (`b` is `int`), so the checker narrows
exactly as `typing.cast` would.

## scope

`cast` is a basedpython-only keyword: a `.py` file using it as an infix produces
a parse error. when the next token cannot start an expression (e.g. `cast = 5`
or `cast(int, a)`), `cast` is parsed as an ordinary identifier so existing
python code that uses `cast` as a name or function call continues to parse
