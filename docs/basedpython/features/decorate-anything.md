# decorating anything

python allows a decorator on a `def` and a `class`. basedpython allows one above
a binding too, and in a type position:

```by
@foo
let a = 1

let b: @meta int = 1
```

transpiles to:

```python
from typing import Annotated, Final

a: Final = foo(1)
b: Final[Annotated[int, meta]] = 1
```

## on a binding

a decorator above a binding means what it means on a definition: it is applied
to what is bound, so the name holds whatever the decorator returned

```by
@wrap
x = 1
```

transpiles to:

```python
x = wrap(1)
```

it reads on every binding that has a value — a plain assignment, an annotated
one, and the [`let` / `var` / `class`](modifiers.md) declarations:

| basedpython               | python output          |
| ------------------------- | ---------------------- |
| `@foo` + `x = 1`          | `x = foo(1)`           |
| `@foo` + `x: int = 1`     | `x: int = foo(1)`      |
| `@foo` + `let a = 1`      | `a: Final = foo(1)`    |
| `@foo` + `var x: int = 1` | `x: int = foo(1)`      |
| `@foo` + `let a: T = 1`   | `a: Final[T] = foo(1)` |

a declaration with no value binds nothing for a decorator to wrap, and carrying
one there is an error:

```by
@foo
let a: int   # error: a declaration with no value binds nothing for a decorator to wrap
```

so is a decorator above anything that is not a definition or a binding:

```by
@foo
if ready:    # error: Expected a definition or a binding after decorator
    run()
```

### the binding keeps its own type

`Annotated[T, …]` is `T`, so the decorator does not change what the name holds —
it records something alongside it

```by
@Field(gt=0)
age: int = 1

reveal_type(age)  # int
```

the decorator is an ordinary expression, evaluated where it is written, so a name
that does not resolve there is an `unresolved-reference`

### more than one

a chain reads innermost first — the decorator written closest to the binding is
the one nearest the type, the same order `@a @b int` puts them in

```by
@outer
@inner
x: int = 1
```

transpiles to:

```python
x: Annotated[int, inner, outer] = 1
```

## on a type

a decorator written in a type position attaches metadata to the type it
precedes. it is what `Annotated` spells

```by
b: @meta int
```

transpiles to:

```python
from typing import Annotated

b: Annotated[int, meta]
```

the type is the one decorated — the decorator says nothing about it, so `b`
above is an `int` and every `Annotated[int, meta]` is assignable to it and it to
them. the decorator is an ordinary expression, evaluated where it is written, so
a name that does not resolve there is an `unresolved-reference`

it reads in every type position, including a nested one:

| basedpython            | python output                     |
| ---------------------- | --------------------------------- |
| `@meta int`            | `Annotated[int, meta]`            |
| `@field(gt=0) int`     | `Annotated[int, field(gt=0)]`     |
| `list[@meta int]`      | `list[Annotated[int, meta]]`      |
| `def f() -> @meta int` | `def f() -> Annotated[int, meta]` |

### precedence

a decoration runs to the end of the type it is written on, the way a decorator on
a `def` covers the whole definition rather than its first line. a
[use-site type modifier](type-modifiers.md) is the other way round — it takes the
operand it precedes and nothing more

```by
a: @meta int | None
```

transpiles to:

```python
a: Annotated[int | None, meta]
```

the postfix `?` is read at the same level as `|`, so it is inside the decoration
too: `@meta int?` is the decorated optional

decorating only part of a union means saying where the decoration ends, which is
what a group does

```by
a: (@meta int) | str
```

transpiles to:

```python
a: Annotated[int, meta] | str
```

the decorator itself is read as a primary expression — a name, an attribute
path, a call, or a subscript — because nothing separates it from the type it
decorates. a parenthesized group after it is therefore ambiguous, and which one
it is depends on what follows: it is the decorator's call arguments when a type
comes after it, and the decorated type when nothing does

| basedpython           | reads as                        |
| --------------------- | ------------------------------- |
| `@meta (int \| None)` | `meta` decorating `int \| None` |
| `@field (gt=0) int`   | `field(gt=0)` decorating `int`  |
| `@meta() int`         | `meta()` decorating `int`       |

### a chain

each decorator adds metadata and none of them changes the type. a chain
collapses into one `Annotated`, whose metadata reads in the order the decorators
apply

```by
a: @x @y int
```

transpiles to:

```python
a: Annotated[int, y, x]
```
