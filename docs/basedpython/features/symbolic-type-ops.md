# symbolic operations in types

ty evaluates operations on literal types — `1 + 1` is the type `Literal[2]`.
basedpython lets the same operations appear in a type position, so an annotation
can be written as an expression and is resolved to its result type:

```by
type A = 1
type B = 2

c: A + B            # `Literal[3]`

let d = 2

e: 1 + typeof d     # `Literal[3]`
```

transpiles to:

```python
from typing import Literal

c: Literal[3]

e: Literal[3]
```

the evaluation reuses ty's value-level operator logic, so it is not limited to
plain `int`s — any operands ty understands work, including type aliases,
[`typeof`](typeof.md), strings, floats, and complex numbers:

```by
s: "foo" + "bar"    # `Literal["foobar"]`
n: 2 ** 8           # `Literal[256]`
f: 1.5 + 1.5        # `3.0`
g: -3 * 2           # `Literal[-6]`
```

## scope

every binary operator except `|` and `&` is treated as a symbolic operation;
those two keep their dedicated meanings (`|` is a [union][pep604] and `&` is an
[intersection](intersection.md)). a folded operation is an ordinary type
expression, so it composes with the other type forms:

```by
xs: list[1 + 1]     # `list[Literal[2]]`
u: 1 + 1 | 4        # `Literal[2] | Literal[4]`
```

an operation ty cannot resolve to a concrete type (for example `+` between two
classes) is left untouched and reported as an invalid type form, the same as any
other unusable annotation.

## polyfill

there is no runtime construct: the operation is resolved at transpile time and
the result type is written directly into the output. `Literal` is imported from
`typing` when a folded result needs it

[pep604]: https://peps.python.org/pep-0604/
