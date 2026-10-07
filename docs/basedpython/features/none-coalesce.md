# none-coalesce operator

`a ?? b` evaluates to `a` when `a is not None`, otherwise to `b`:

```by
name = user.display_name ?? "anonymous"
```

transpiles to:

```python
name = __by_t_0__ if (__by_t_0__ := user.display_name) is not None else "anonymous"
```

a compound left operand is bound to a temp by the walrus so it is evaluated
exactly once. a bare name needs no temp and is repeated directly — `a ?? b` is
`a if a is not None else b`

python refuses the walrus in some places: a comprehension's iterable, a
comprehension in a class body, an annotation, and a generic definition's type
parameters, bases and `type` value. there the temp is a comprehension variable
instead, which still evaluates the left operand once and the right one only when
it is needed:

```by
names = [n for n in user.nicknames ?? []]
```

transpiles to:

```python
names = [n for n in ([__by_t_0__ for __by_t_0__ in (user.nicknames,) if __by_t_0__ is not None] or [[]])[0]]
```

## semantics

`??` tests `is not None` (identity) — not falsiness. an empty string, zero,
or an empty list is *not* coalesced. only `None` triggers the fallback

## precedence

`??` has the same precedence as `or` and groups left to right with it. it binds
looser than `and`, `not`, comparisons and arithmetic, and tighter than a
conditional expression:

```by
a ?? b or c   # (a ?? b) or c
a or b ?? c   # (a or b) ?? c
a ?? b and c  # a ?? (b and c)
```

parenthesize when mixing it with boolean operators

## interaction with `?.`

see [optional chaining](optional-chaining.md). `??` and `?.` compose; complex
left operands are evaluated at most once even when chained
