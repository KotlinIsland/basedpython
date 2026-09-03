# tuple type literals

a parenthesized tuple in an annotation position is rewritten to `tuple[...]`:

```by
point: (int, int)
record: (int, str, float)
nested: (int, (str, float))
maybe: (int, str) | None

def origin() -> (int, int):
    return (0, 0)
```

transpiles to:

```python
point: tuple[int, int]
record: tuple[int, str, float]
nested: tuple[int, tuple[str, float]]
maybe: tuple[int, str] | None

def origin() -> tuple[int, int]:
    return (0, 0)
```

## unpacking

a `*` element splices the tuple it names in, rather than nesting it as a single
field:

```by
type Pair = (int, str)
type Triple = (bool, *Pair)   # (bool, int, str)
type Same = (*Pair)           # (int, str)
```

the star has already made this a tuple, so the lone-element form needs no
trailing comma. a `type` alias is resolved first, so a named tuple type unpacks
exactly as the tuple it stands for — that holds for python's `tuple[*Pair]` and
`Unpack[Pair]` spellings too

`*A` and the [variadic `*: T`](callable.md) parse to the same shape but mean
different things: `*: T` annotates every field, while `*A` splices one tuple in.
what tells them apart is the `:`

a variadic whose annotation is itself an unpack names the whole run of fields
rather than typing each one, which is the same reading
[a callable](callable.md#variadic-args) gives it — so it splices exactly as the
bare form does:

```by
type Pair = (int, str)
type Same = (*: *Pair)          # (int, str)
type Leading = (bool, *: *Pair) # (bool, int, str)
```

unpacking lowers to python's own `*` spelling, so it inherits python's runtime
rule: `*` on a `type` alias is only evaluatable lazily. that covers the type
positions the form is written in — another `type` alias, or any annotation once
`from __future__ import annotations` is on — but an eagerly evaluated annotation
that unpacks an alias raises at import, exactly as the same annotation would in
python. unpacking a tuple type or a `TypeVarTuple` directly has no such limit

## syntax

```text
tuple_type ::= "(" element ("," element)* [","] ")"
element    ::= type | "*" type
```

a parenthesized list of one or more elements — trailing comma allowed. a
single-element form requires the trailing comma to disambiguate from a
parenthesized expression: `(int,)` is `tuple[int]`, while `(int)` is
just `int`. an unpacked element is already unambiguous, so `(*A)` needs none

## scope

rewriting fires in syntactic type contexts only: parameter annotations,
return-type annotations, `AnnAssign` targets, type aliases, and subscript
slices that are themselves type contexts. value-context tuples (`x = (1, 2)`)
are untouched

## composition

the rule recurses into surrounding type forms — unions, callables,
generics, intersections — so any tuple type expression nested inside is
also rewritten:

```by
fns: list[(int) -> (str, int)]
# → list[Callable[[int], tuple[str, int]]]
```

## relation to anonymous named tuples

if any element in the parenthesized list uses `name : type` form, the
expression is recognised as an [anonymous named tuple](anonymous-named-tuple.md)
instead. tuple-type rewrite and anon-NT rewrite are exclusive: a tuple
either has all-positional fields (becomes `tuple[...]`) or contains at
least one named field (becomes a synthesized `NamedTuple` class)
