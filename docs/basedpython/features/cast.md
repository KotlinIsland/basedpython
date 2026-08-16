# cast

basedpython adds a `cast` infix soft keyword for inline type casts:

```by
b = a cast object
```

`cast` is **static**: it reinterprets the value without looking at it, so it
transpiles to a plain `typing.cast` and costs nothing at runtime.

that only works when the claim is already true, so `cast` is limited to a
**widening** — the value must already be the type it is being taken as. casting
the other way, from `object` down to `int`, asserts something nothing verifies:

```by
def f(a: object):
    b = a cast int  # error: cast from `object` to `int` is not a widening
```

a downcast has to say what happens when the value turns out not to match. that
is what [`cast!` and `cast?`](checked-cast.md) are for — `cast!` raises, `cast?`
yields `None`:

```by
def f(a: object):
    b = a cast! int   # int, raises on a mismatch
    c = a cast? int   # int | None
```

## syntax

`cast` is an infix soft keyword: `<value> cast <type>`. the left operand is
the value being cast, the right operand is the target type

```by
b = a cast int | str             # cast to int | str
f(a cast object)                 # f(a cast to object)
```

the inferred type is the target type (`b` is `int | str`), so the checker
narrows exactly as `typing.cast` would.

## what counts as a widening

the value's type must already be the target: `int cast object` is fine, so is a
cast to the value's own type, or to a union containing it. anything else — a
downcast to a subtype, a sidecast between unrelated types, or a cast from a
gradual `Any` — is rejected by the `unsound-cast` check, whose fix rewrites the
keyword to `cast!`.

`Any` gets no exemption. nothing at all is known about such a value, which is
exactly when checking it is worth the runtime cost.

## scope

`cast` is a basedpython-only keyword: a `.py` file using it as an infix produces
a parse error. when the next token cannot start an expression (e.g. `cast = 5`
or `cast(int, a)`), `cast` is parsed as an ordinary identifier so existing
python code that uses `cast` as a name or function call continues to parse
