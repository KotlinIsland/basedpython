# frozen container displays

python spells `{…}` for the mutable containers only. basedpython lets the
declared type supply the frozen reading, so a written-out display stands for a
`frozenset` or a `frozendict` where one is asked for:

```by
b: frozenset[int] = {1, 2}
```

transpiles to:

```python
b: frozenset[int] = frozenset({1, 2})
```

`frozendict` is the same, and needs python 3.15 — that is the version the
builtin arrives in:

```by
a: frozendict[str, int] = {"x": 1}
```

→

```python
a: frozendict[str, int] = frozendict({"x": 1})
```

## `{}` is the empty set where a set is asked for

python has no empty-set display: `{}` is a dict, and `set()` is the only way to
write the other one. where a set is declared, `{}` reads as one:

```by
d: set[int] = {}
e: frozenset[int] = {}
```

→

```python
d: set[int] = set()
e: frozenset[int] = frozenset()
```

the display is dropped rather than wrapped: constructing from an empty display
needs no argument, so nothing builds the dict `{}` would have been. the same
goes for `frozendict[str, str] = {}`, which is `frozendict()`.

everywhere else `{}` keeps python's meaning, so `plain: dict[str, int] = {}` is
still an empty dict and is left alone.

## where it applies

this is [`__of__`](conversions.md#__of__), so it applies wherever a conversion
site does — an argument, a `return`, an annotated assignment, an element of
another display:

```by
def takes(fs: frozenset[str]) -> None: ...

takes({"a"})

def gives() -> frozenset[int]:
    return {1, 2}

nested: list[frozenset[int]] = [{1}, {2}]
```

and, being `__of__`, it applies only to a display **written out at the site**.
a name that happens to hold a set is not one:

```by
t = {1, 2}
b: frozenset[int] = t   # error: `set[int]` is not assignable to `frozenset[int]`
```

that restriction is the whole point: the braces are in the source, so wrapping
them is honest. a `set` you already have is converted by writing
`frozenset(t)`, which is what the wrap would have said anyway.

## the conversion comes from an extension

nothing is special-cased about these three types. `frozenset`, `frozendict` and
`set` are given `__of__` by an [extension](extensions.md) that basedpython
declares for you, the same way any extension adds a member to a type it does not
own:

```by
extension frozenset:
    class def __of__(cls, value: set[Element]) -> frozenset[Element]
```

so the member is real, and spelling the call out by hand means exactly what the
implicit one does:

```by
b = frozenset.__of__({1, 2})
```

→

```python
b = frozenset({1, 2})
```

an extension you write can supply `__of__` or `__from__` for a type you do not
own in the same way — see
[conversions from an extension](conversions.md#conversions-from-an-extension).

## element-wise conversion does not apply

a display whose *elements* need converting converts element by element, but that
leaves the display itself alone — `{1, 2}` is still a `set` however its elements
are wrapped. so a frozen target whose element type does not match is an ordinary
error rather than a half-repair:

```by
class Meters:
    init(value: float)

    @classmethod
    def __of__(cls, value: int) -> Self:
        return cls(float(value))

lengths: list[Meters] = [1, 2]      # converts: a list display for a list
frozen: frozenset[Meters] = {1, 2}  # error: `set[int]` is not assignable to `frozenset[Meters]`
```

write the conversion where it happens:

```by
frozen: frozenset[Meters] = frozenset({Meters.__of__(1), Meters.__of__(2)})
```

## a frozen display inside a frozen display

for the same reason, and because
[the repair is single-step](conversions.md#conversion-sites), a display nested
directly inside another one that also has to be frozen is not converted — the
outer conversion is checked against what the inner display *is*, not what it
would become:

```by
ok: list[frozenset[int]] = [{1}]                    # the outer display is already a list
no: frozendict[str, frozenset[int]] = {"a": {1}}    # error
```

spell the inner one:

```by
yes: frozendict[str, frozenset[int]] = {"a": frozenset({1})}
```
