# basedpython: a decorated type

A decorator may also be written in a type position, where it attaches metadata to the type it
precedes — `@meta int` is what `Annotated[int, meta]` spells. The type is the one decorated; the
decorator is metadata, and says nothing about it.

```toml
[environment]
python-version = "3.12"
```

## the type is the one decorated

```by
meta = "units: metres"

def f(a: @meta int) -> None:
    reveal_type(a)  # revealed: int
```

## the decoration is checked like any other annotation

```by
meta = "units: metres"

b: @meta int = "not an int"  # error: [invalid-assignment]
```

## it takes the whole type written after it

Unlike the `literal` and `final` use-site modifiers, which take the type written next to them and
nothing more, a decoration runs to the end of the type — so a union after it is inside it, and needs
no group of its own.

```by
meta = "units: metres"

def f(x: @meta int | str) -> None:
    reveal_type(x)  # revealed: int | str
```

The postfix `?` is read at the same level as `|`, and the decoration runs past both.

```by
meta = "units: metres"

def f(x: @meta int?) -> None:
    reveal_type(x)  # revealed: int | None
```

## a decoration that is one arm of a union is grouped

The decoration would otherwise run on and take the rest of the union with it, so writing only part
of a union as decorated means saying where it ends.

```by
meta = "units: metres"

def f(x: (@meta int) | str) -> None:
    reveal_type(x)  # revealed: int | str
```

## a parenthesized group decorates as a whole

The decorator is read greedily, so a `(` after it is its call arguments — unless nothing follows the
call to decorate, in which case the group was the type.

```by
meta = "units: metres"

def f(x: @meta (int | None)) -> None:
    reveal_type(x)  # revealed: int | None
```

## nested in a type argument

```by
meta = "units: metres"

c: list[@meta int] = []

reveal_type(c)  # revealed: list[int]
```

## a chain of decorators

Each one adds metadata; none of them changes the type.

```by
first = "first"
second = "second"

def f(d: @first @second int) -> None:
    reveal_type(d)  # revealed: int
```

## the decorator is an ordinary expression

It is evaluated where it is written, so a name that does not resolve is reported there.

```by
e: @nope int = 1  # error: [unresolved-reference]
```

## a call as the decorator

The decorator may be any primary expression, so a call to a metadata factory reads as one.

```by
def units(name: str) -> str:
    return name

def f(g: @units("metres") int) -> None:
    reveal_type(g)  # revealed: int
```

## it means what `Annotated` means

A decorated type and the `Annotated` it spells describe the same type, so a value of either is
assignable to the other.

```by
from typing import Annotated

meta = "units: metres"

def f(h: @meta int, i: Annotated[int, meta]) -> None:
    j: Annotated[int, meta] = h
    k: @meta int = i
    reveal_type(j)  # revealed: int
    reveal_type(k)  # revealed: int
```

## a decorated return type

```by
meta = "units: metres"

def f() -> @meta int:
    return 1

reveal_type(f())  # revealed: int
```
