# basedpython: template literal types

an f-string in a type position is the set of strings its pattern produces. each hole stands for
`str(x)` over the values of the hole's own type, and the text between the holes is fixed.

```toml
[environment]
python-version = "3.13"
```

## a string literal inhabits a pattern it matches

```by
a: f"asdf{int}fdsa" = "asdf5fdsa"
reveal_type(a)  # revealed: "asdf5fdsa"
```

## a string literal that does not match is rejected

```by
# error: [invalid-assignment] "Object of type `"asdfXfdsa"` is not assignable to `f"asdf{int}fdsa"`"
a: f"asdf{int}fdsa" = "asdfXfdsa"
```

## an int hole is the rendering `str` actually produces

a negative number renders with its sign, and no `int` renders with a leading zero.

```by
a: f"a{int}b" = "a-7b"

# error: [invalid-assignment]
b: f"a{int}b" = "a07b"
```

## a pattern with no holes left is a string literal

every hole here renders to one string, so nothing is left to stand for.

```by
def f(a: f"a{1}b", b: f"{"q"}{2}") -> None:
    reveal_type(a)  # revealed: "a1b"
    reveal_type(b)  # revealed: "q2"
```

## a union hole distributes

```by
def f(a: f"a{1 | 2}b", b: f"{bool}") -> None:
    reveal_type(a)  # revealed: "a1b" | "a2b"
    reveal_type(b)  # revealed: "True" | "False"
```

## a lone hole that is already a set of strings is that type

```by
def f(a: f"{str}", b: f"{Character}") -> None:
    reveal_type(a)  # revealed: str
    reveal_type(b)  # revealed: Character
```

## a hole spelled as a type alias is the type it stands for

a hole is read for what it means rather than for how it was written, so an alias distributes the
same way the type it names would.

```by
type Name = "foo" | "bar"

def f(a: f"the {Name}") -> None:
    reveal_type(a)  # revealed: "the foo" | "the bar"

# error: [invalid-assignment] "Object of type `"the asdf"` is not assignable to `"the foo" | "the bar"`"
b: f"the {Name}" = "the asdf"
```

## an alias is followed as far as it goes

a union written in a type expression keeps the names its arms were written with, so the type a hole
stands for can be an alias of an alias. every step is followed, and a pattern reached that way is
spliced in just as one written directly would be.

```by
type Inner = "foo" | "bar"
type Outer = Inner | "baz"

type Version = f"v{int}"
type Tagged = Version | "untagged"

def f(a: f"the {Outer}", b: f"[{Tagged}]") -> None:
    reveal_type(a)  # revealed: "the foo" | "the bar" | "the baz"
    reveal_type(b)  # revealed: f"[v{int}]" | "[untagged]"
```

## an alias that names itself is followed only once

```by
type Loop = Loop | "q"  # error: [cyclic-type-alias-definition] "Cyclic definition of `Loop`"

type Left = Right | int  # error: [cyclic-type-alias-definition] "Cyclic definition of `Left`"
type Right = Left | str  # error: [cyclic-type-alias-definition] "Cyclic definition of `Right`"

def f(a: f"a{Loop}b", b: f"a{Right}b") -> None:
    reveal_type(a)  # revealed: "aqb"
    reveal_type(b)  # revealed: f"a{str}b"
```

## an alias of a type that is not a union is still that type

resolving a hole is not a distribution — an alias naming one type leaves a pattern that is still a
pattern, spelled with the type rather than with the alias.

```by
type Text = str

def f(a: f"a{Text}b") -> None:
    reveal_type(a)  # revealed: f"a{str}b"
```

## a `None` hole renders as `str(None)` does

```by
def f(a: f"{None}") -> None:
    reveal_type(a)  # revealed: "None"
```

## a hole that cannot be inhabited empties the pattern

```by
from typing import Never

def f(a: f"a{Never}b") -> None:
    reveal_type(a)  # revealed: Never
```

## a narrower hole is assignable to a wider one

```by
def f(x: f"a{int}b", y: f"a{str}b") -> None:
    ok: f"a{str}b" = x

    # error: [invalid-assignment] "Object of type `f"a{str}b"` is not assignable to `f"a{int}b"`"
    bad: f"a{int}b" = y
```

## a pattern is a `str`

```by
def f(a: f"x{int}") -> None:
    b: str = a
    reveal_type(a.upper())  # revealed: str
    reveal_type(len(a))  # revealed: int
```

## a pattern that cannot be empty is always truthy

```by
def f(a: f"x{str}", b: f"{int}") -> None:
    reveal_type(bool(a))  # revealed: True
    reveal_type(bool(b))  # revealed: True
```

## a pattern built only from literal strings is a `LiteralString`

```by
from typing import LiteralString

def f(a: f"a{LiteralString}b", b: f"a{int}b") -> None:
    ok: LiteralString = a

    # error: [invalid-assignment]
    bad: LiteralString = b
```

## equality against a string the pattern cannot produce is statically false

```by
def f(a: f"a{int}b") -> None:
    reveal_type(a == "zzz")  # revealed: False
    reveal_type(a == "a1b")  # revealed: bool
```

## equality narrows to the literal

```by
def f(a: f"x{int}") -> None:
    if a == "x5":
        reveal_type(a)  # revealed: "x5"
    if a == "zzz":
        reveal_type(a)  # revealed: Never
```

## a hole may be a type parameter

```by
def render[T](x: T) -> f"a{T}b":
    return f"a{x}b"

reveal_type(render(1))  # revealed: "a1b"
reveal_type(render("q"))  # revealed: "aqb"

class Box[T]:
    def __init__(self, value: T) -> None:
        self.value = value

    def label(self) -> f"a{T}b":
        return f"a{self.value}b"

reveal_type(Box[int](1).label())  # revealed: f"a{int}b"
```

## an f-string value is the pattern it spells

```by
def route(path: f"/{str}", n: int) -> f"{str}-ok":
    return f"{path}-ok"

def version(n: int) -> f"v{int}":
    return f"v{n}"

def wrong(s: str) -> f"v{int}":
    # error: [invalid-return-type] "expected `f"v{int}"`, found `f"v{str}"`"
    return f"v{s}"
```

## an f-string value is a pattern with no annotation in sight

```by
def f(name: str, n: int) -> None:
    a = f"hello {name}"
    reveal_type(a)  # revealed: f"hello {str}"

    b = f"asdf_{n}"
    reveal_type(b)  # revealed: f"asdf_{int}"

    c = f"x{1}"
    reveal_type(c)  # revealed: "x1"
```

## a pattern inferred from a value widens where a string literal would

the pattern is worth keeping where it can be read back, and worth losing where the context could not
hold it — the element type of a mutable list is invariant, so it widens exactly as `["abc"]` widens
to `list[str]`.

```by
def f(n: int) -> None:
    xs = [f"a{n}"]
    reveal_type(xs)  # revealed: list[str]
    xs.append("zzz")

    ys: list[str] = [f"a{n}"]
    zs: str = f"a{n}"
```

## a pattern written in a type expression does not widen

```by
def f(a: f"a{int}") -> None:
    reveal_type(a)  # revealed: f"a{int}"
```

## a hole cannot carry a conversion or a format specifier

```by
# error: [invalid-type-form] "A hole in a template literal type cannot have a conversion or a format specifier"
def f(a: f"{int!r}") -> None: ...
```

## a format specifier in a value drops back to `str`

```by
def f(n: int) -> f"v{int}":
    # error: [invalid-return-type] "expected `f"v{int}"`, found `str`"
    return f"v{n:>3}"
```

## braces are escaped the way an f-string escapes them

```by
def f(a: f"{{lit}}{int}") -> None:
    reveal_type(a)  # revealed: f"{{lit}}{int}"
```

## implicit concatenation joins into one pattern

```by
def f(a: f"a" f"{int}" "b") -> None:
    reveal_type(a)  # revealed: f"a{int}b"
```

## a python file keeps the standard error

```py
# error: [invalid-type-form] "F-strings are not allowed in type expressions"
# error: [implicit-object-repr]
a: f"asdf{int}fdsa" = "asdf5fdsa"
```
