# flexible keyword argument names

a keyword argument's name may be written as a dotted path or as a string
literal, not just as a bare identifier:

```by
def f(**kwargs: int) -> None: ...

f(foo.bar=1, "content-type"=2)
```

transpiles to:

```python
def f(**kwargs: int) -> None: ...

f(**{"foo.bar": 1}, **{"content-type": 2})
```

the name is the path's text or the string's *value*, so a call can name a
keyword that python has no way to spell — which is how it reaches a `**kwargs`
entry whose key is not a python identifier

## when to reach for it

any api that takes its options as a mapping rather than as declared parameters:

```by
query.filter("user.name"=name, "created.year"=2026)
config.update(logging.level="debug")
```

the alternative python offers is `**{...}`, which reads as plumbing rather than
as arguments and hides the keys from the call's shape

## what the name may be

a dotted path is a name, not something to evaluate — `foo` is not looked up:

```by
f(foo.bar.baz=1)    # the key is "foo.bar.baz"
```

a string literal's escapes are decoded, so the key is whatever the string holds:

```by
f("a\tb"=1)         # the key is "a<tab>b"
f(""=2)             # the key is ""
```

an f-string, a bytes literal and an implicitly concatenated string are not
names, and neither is anything that has to be evaluated (`f(a[0]=1)`,
`f(g().b=1)`) — each is the syntax error it already was

a name may not span lines, because it is written back exactly as spelled

## what it lowers to

each name python cannot spell becomes its own single-entry mapping, spliced
where it was written, so the arguments stay in source order and nothing else
about the call changes:

```by
f(a, b=1, c.d=2, e=3)   # → f(a, b=1, **{"c.d": 2}, e=3)
```

a name python *can* spell only loses its quotes:

```by
f("timeout"=1)          # → f(timeout=1)
```

python's grammar allows `f(a=1, *rest)` but not `f(**d, *rest)`, so when a
starred argument follows a lowered name the whole argument list is re-emitted
with the positional arguments first:

```by
f(a.b=1, *rest)         # → f(*rest, **{"a.b": 1})
```

this is not a reordering in any observable sense: the language reference says a
`*expression` in a call is processed before the keyword arguments, so cpython
already evaluates `rest` first in `f(a=1, *rest)`

## checking

a flexible name is checked like any other keyword argument. it matches a
parameter of that name if there is one, and otherwise goes to `**kwargs`:

```by
def f(**kwargs: int) -> None: ...

f("a b"=1)      # ok
f("a b"="x")    # error: expected `int`, found `Literal["x"]`
```

with a [keyword-variadic pack](keyword-variadic.md) the name becomes a field
name, so it is carried through to whatever reads the pack

## formatting

a quoted name keeps the quote characters it was written with, rather than being
normalised to the configured quote style — like an identifier, it is printed
exactly as spelled

## see also

- [keyword-variadic packs](keyword-variadic.md) — typing the `**kwargs` a
    flexible name lands in
- [keyword arguments in subscripts](kw-subscript.md) — the same idea one level
    up, in a subscription
