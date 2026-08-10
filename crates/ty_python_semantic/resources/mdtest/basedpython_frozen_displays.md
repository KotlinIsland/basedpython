# basedpython frozen container displays

Python spells `{…}` for the mutable containers only, so the declared type supplies the frozen
reading: a written-out display converts through `__of__` like any other literal. The prelude
declares those conversions as `extension` members, which is what lets a builtin offer one.

## a set display satisfies a declared `frozenset`

```by
b: frozenset[int] = {1, 2}
reveal_type(b)  # revealed: frozenset[int]
```

## a dict display satisfies a declared `frozendict`

```toml
[environment]
python-version = "3.15"
```

```by
a: frozendict[str, int] = {"x": 1}
reveal_type(a)  # revealed: frozendict[str, int]
```

## an empty display satisfies a declared `frozendict`

```toml
[environment]
python-version = "3.15"
```

```by
a: frozendict[str, str] = {}
reveal_type(a)  # revealed: frozendict[str, str]
```

## `{}` is the empty set where a set is declared

Python has no empty-set display, so `{}` reads as one when that is what was asked for.

```by
d: set[int] = {}
reveal_type(d)  # revealed: set[int]
```

## `{}` is the empty set where a `frozenset` is declared

```by
e: frozenset[int] = {}
reveal_type(e)  # revealed: frozenset[int]
```

## a populated dict display is not a set

The empty display is typed exactly at a conversion site, so `{}` cannot be confused with a display
whose keys merely happen to be unknown.

```by
def keys() -> dict[str, int]:
    return {}

d: set[str] = {"a": 1}  # error: [invalid-assignment]
```

## a name holding a set does not convert

The brackets have to be in the source — that is what `__of__` means.

```by
t = {1, 2}
b: frozenset[int] = t  # error: [invalid-assignment]
```

## a frozen display converts in an argument position

```by
def takes(fs: frozenset[str]) -> None: ...

takes({"a"})
```

## a frozen display converts in a return position

```by
def gives() -> frozenset[int]:
    return {1, 2}
```

## a frozen display converts as an element of another display

```by
nested: list[frozenset[int]] = [{1}, {2}]
reveal_type(nested)  # revealed: list[frozenset[int]]
```

## the element type is checked

```by
b: frozenset[int] = {"a"}  # error: [invalid-assignment]
```

## a `.py` file gets none of this

Conversions are basedpython's, so python keeps python's meaning for a display.

```py
b: frozenset[int] = {1, 2}  # error: [invalid-assignment]
```

## a frozen display inside a frozen display is not converted

The repair is single-step, so the outer conversion is checked against what the inner display is, not
against what it would become.

```toml
[environment]
python-version = "3.15"
```

```by
no: frozendict[str, frozenset[int]] = {"a": {1}}  # error: [invalid-assignment]
```

## the inner display spelled out converts

```toml
[environment]
python-version = "3.15"
```

```by
yes: frozendict[str, frozenset[int]] = {"a": frozenset({1})}
reveal_type(yes)  # revealed: frozendict[str, frozenset[int]]
```

## an empty display is a set in a return position

```by
def f() -> set[int]:
    return {}

def g() -> frozenset[int]:
    return {}

reveal_type(f())  # revealed: set[int]
reveal_type(g())  # revealed: frozenset[int]
```

## an empty display in a return position stays a dict where one is declared

```by
def keys() -> dict[str, int]:
    return {}

reveal_type(keys())  # revealed: dict[str, int]
```
