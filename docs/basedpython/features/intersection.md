# intersection types

basedpython adds an `&` operator for intersection types in annotation positions,
mirroring the existing `|` for unions:

```by
def render(x: Drawable & Serializable) -> bytes: ...

handlers: list[HasName & HasId]

cb: (A & B) | C
```

transpiles to:

```python
def render(x: Intersection[Drawable, Serializable]) -> bytes: ...

handlers: list[Intersection[HasName, HasId]]

cb: Intersection[A, B] | C
```

## semantics

`A & B` is the type of values that satisfy both `A` *and* `B`. left-associative
chains are flattened: `A & B & C` becomes `Intersection[A, B, C]` rather than
nested. intersections compose with unions by precedence: `&` binds tighter than
`|`, so `A & B | C` parses as `(A & B) | C`

the keywords `and` / `or` are accepted as alternate spellings of `&` / `|` —
see [`or` / `and` type operators](or-and-types.md)

## scope

the `&` operator is recognized only in syntactic type positions: annotations,
return types, type aliases, and subscript slices that are themselves type
contexts. bitwise AND in value expressions is untouched:

```by
x = A & B   # bitwise AND — unchanged
a: A & B    # intersection — rewritten
```

## polyfill

`Intersection` is imported from `ty_extensions` (basedpython's type-extension
module). there is no native runtime equivalent — intersections are a
type-checker-only construct, evaluated by ty
