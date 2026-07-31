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

## a type parameter operand

an operand may be a type parameter, which is not known where the annotation is
written. the operation is then kept symbolic rather than collapsed to the
parameter's bound, and re-evaluated against each specialization:

```by
class Array[Dim: int]

def extend[Dim: int](a: Array[Dim]) -> Array[Dim + 1]:
    return a

def f(data: Array[5]):
    reveal_type(extend(data))       # `Array[6]`
    reveal_type(extend(extend(data)))  # `Array[7]`
```

collapsing `Dim + 1` to `int` at the definition would throw the relationship
away and infer `Array[int]` at every call site.

### the body is checked against the operation

`I + 1` names one value per specialization, so a body has to produce *that*
value — checking against the reduced form would ask only for an `int`:

```by
def succ[I: int](i: I) -> I + 1:
    return i + 1        # ok

def wrong[I: int](i: I) -> I + 1:
    return i            # error: expected `I + 1`, found `I`
```

arithmetic on values is kept symbolic for this, so `i + 1` has the type `I + 1`
rather than `int`. two expressions naming the same value need not be written the
same way: operands may be commuted, constants folded together, terms cancelled,
and calls whose own return type is symbolic composed.

```by
def commuted[I: int](i: I) -> I + 1:
    return 1 + i

def rearranged[I: int](i: I) -> I * 2 + 1:
    return 1 + 2 * i

def twice[I: int](i: I) -> I + 2:
    return succ(succ(i))
```

`+`, `-`, `*` and the unary operators are decided this way. a comparison, a
method call or an [attribute type](attribute-types.md) has no such decision
procedure, so a body annotated with one is checked only against the type the
operation reduces to. a body that is correct for a reason the checker cannot see
takes the escape hatch every other unprovable assignment takes:

```by
def from_len[I: int](i: I, xs: list[int]) -> I + 1:
    return len(xs) cast I + 1
```

## polyfill

there is no runtime construct: the operation is resolved at transpile time and
the result type is written directly into the output. `Literal` is imported from
`typing` when a folded result needs it

[pep604]: https://peps.python.org/pep-0604/
