# basedpython: unpacking a tuple type into a tuple type

A `*` element splices the tuple it names into the tuple being written, rather than nesting it as a
single field. It resolves through a `type` alias, so a named tuple type unpacks exactly as the tuple
it stands for.

```toml
[environment]
python-version = "3.12"
```

## a lone unpacked element needs no comma

The star has already said this is a tuple, so python's disambiguating trailing comma is noise.

```by
type Pair = (int, str)
type Spliced = (*Pair)

def f(x: Spliced):
    reveal_type(x)  # revealed: (int, str)
```

## the trailing comma is still accepted

```by
type Pair = (int, str)
type Spliced = (*Pair,)

def f(x: Spliced):
    reveal_type(x)  # revealed: (int, str)
```

## unpacking after a prefix

```by
type Pair = (int, str)
type Triple = (bool, *Pair)

def f(x: Triple):
    reveal_type(x)  # revealed: (bool, int, str)
```

## unpacking before a suffix

```by
type Pair = (int, str)
type Triple = (*Pair, bool)

def f(x: Triple):
    reveal_type(x)  # revealed: (int, str, bool)
```

## unpacking twice

```by
type Pair = (int, str)
type Quad = (*Pair, *Pair)

def f(x: Quad):
    reveal_type(x)  # revealed: (int, str, int, str)
```

## the `tuple[...]` spelling unpacks an alias too

A `type` alias resolves to the tuple it names, so python's spelling splices the same way.

```by
type Pair = (int, str)
type Spliced = tuple[*Pair]

def f(x: Spliced):
    reveal_type(x)  # revealed: (int, str)
```

## `Unpack` is the same unpack

```by
from typing import Unpack

type Pair = (int, str)
type Triple = (bool, Unpack[Pair])

def f(x: Triple):
    reveal_type(x)  # revealed: (bool, int, str)
```

## a tuple type unpacks without an alias in the way

An inline tuple type needs no alias to resolve through, so it works in an eagerly evaluated
annotation as well.

```by
def f(x: (bool, *tuple[int, str])):
    reveal_type(x)  # revealed: (bool, int, str)
```

## only a tuple type or `TypeVarTuple` can be unpacked

```by
type NotATuple = int

# error: [invalid-type-form] "`*` can only unpack a tuple type or `TypeVarTuple`"
type Bad = (*NotATuple)
```

## `*: T` stays the homogeneous variadic

`*: T` annotates every field rather than splicing one tuple in, so the two spellings stay distinct
even though they parse to the same shape.

```by
def f(x: (*: int)):
    reveal_type(x)  # revealed: (*: int)
```

## a `TypeVarTuple` still unpacks

```by
def f[*Args](x: (bool, *Args)) -> None: ...

f[int, str]((True, 1, "a"))
# error: [invalid-argument-type] "Expected `(bool, int, str)`, found `(True, 1, 2)`"
f[int, str]((True, 1, 2))
```
