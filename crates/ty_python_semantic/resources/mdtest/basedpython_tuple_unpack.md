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

## a variadic annotated with an unpack names the whole run

`*: *Args` is the variadic whose annotation is itself an unpack, which is the same spelling the
[callable form](basedpython_callable.md) uses. It names the fields the tuple `Args` stands for
rather than typing each field with `Args`, so it splices exactly as a bare `*Args` does.

```by
type Pair = (int, str)
type Same = (*: *Pair)
type Named = (*args: *Pair)

def f(x: Same):
    reveal_type(x)  # revealed: (int, str)

def g(x: Named):
    reveal_type(x)  # revealed: (int, str)
```

## an unpacked variadic keeps its prefix and suffix

```by
type Pair = (int, str)
type Leading = (bool, *: *Pair)
type Trailing = (*: *Pair, bool)

def f(x: Leading):
    reveal_type(x)  # revealed: (bool, int, str)

def g(x: Trailing):
    reveal_type(x)  # revealed: (int, str, bool)
```

## an unpacked variadic accepts a tuple of the spliced shape

```by
type T[*Ts] = (*: *Ts)

a: T[int] = (1,)
# error: [invalid-assignment]
b: T[int] = (1, 2)
```

## a `TypeVarTuple` fills an unpacked variadic

Unpacking a `TypeVarTuple` directly has none of the lazy-evaluation limits a `type` alias brings, so
the variadic can be written in the annotation itself.

```by
def f[*Args](x: (*: *Args)) -> None: ...

f[int, str]((1, "a"))
# error: [invalid-argument-type] "Expected `(int, str)`, found `(1, 2)`"
f[int, str]((1, 2))
```

## a `TypeVarTuple` still unpacks

```by
def f[*Args](x: (bool, *Args)) -> None: ...

f[int, str]((True, 1, "a"))
# error: [invalid-argument-type] "Expected `(bool, int, str)`, found `(True, 1, 2)`"
f[int, str]((True, 1, 2))
```
