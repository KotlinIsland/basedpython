# `or` / `and` type operators

basedpython accepts the keywords `or` and `and` in annotation positions as
spellings of union and intersection — `A or B` means `A | B`, `A and B`
means `A & B`:

```by
def parse(raw: str or bytes) -> int or None: ...

handlers: list[HasName and HasId]

cb: A and B or C
```

transpiles to:

```python
def parse(raw: str | bytes) -> int | None: ...

handlers: list[Intersection[HasName, HasId]]

cb: Intersection[A, B] | C
```

## semantics

`A or B` is the union of `A` and `B`; `A and B` is the type of values that
satisfy both `A` *and* `B` — exactly the semantics of `|` and `&` from
[intersection types](intersection.md). the keywords follow python's boolean
precedence, which mirrors the bitwise one: `and` binds tighter than `or`, so
`A and B or C` is `(A & B) | C`

n-ary chains flatten: `A and B and C` becomes `Intersection[A, B, C]` rather
than nested, and `A or B or C` becomes one `A | B | C` union. the keyword and
symbolic spellings mix freely — `A & B and C` is one three-arm intersection,
`A | B or C` one three-arm union. parentheses compose as usual:
`(A or B) and C` is `Intersection[A | B, C]`

## scope

the keywords are recognized only in syntactic type positions: annotations,
return types, type aliases, and subscript slices that are themselves type
contexts. boolean `or` / `and` in value expressions is untouched:

```by
x = a or b    # boolean or — unchanged
a: A or B     # union — rewritten
```

## polyfill

`or` lowers to the native `|` union, which needs no import. `and` lowers to
`Intersection` from `ty_extensions`, exactly like the `&` operator — see
[intersection types](intersection.md)

## round-tripping

`or` / `and` are alternate input spellings, not the canonical surface form.
reverse transpilation re-sugars `Intersection[…]` to `&` and leaves `|`
as-is, so a round-trip emits the symbolic operators
