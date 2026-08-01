# basedpython: anonymous named tuple type expressions

In basedpython, `(name: T, name: T, ...)` is sugar for an inline `typing.NamedTuple` subclass. ty
synthesizes a `NamedTuple` class per unique shape (matching the transpiler) so attribute access on
the named fields resolves to the field's declared type. Identity is shape-based: two structurally
identical anonymous named tuples in the same file resolve to the same class.

## Type-expression position

### As a parameter annotation — named field access

```by
def f(x: (name: str, age: int)) -> None:
    reveal_type(x.name)  # revealed: str
    reveal_type(x.age)  # revealed: int
```

### As a return annotation — field access on the returned value

```by
def g() -> (name: str, age: int):
    return ("asdf", 1)

reveal_type(g().name)  # revealed: str
reveal_type(g().age)  # revealed: int
```

### As a variable annotation

```by
def f(s: str, n: int) -> None:
    a: (name: str, age: int) = (s, n)
    reveal_type(a.name)  # revealed: str
    reveal_type(a.age)  # revealed: int
```

### Single-field anonymous named tuple

```by
def h(x: (only: int)) -> None:
    reveal_type(x.only)  # revealed: int
```

### Mixed positional and named type fields

```by
def f(x: (int, name: str)) -> None:
    # Positional fields get synthetic names `arg0`, `arg1`, ...
    reveal_type(x.arg0)  # revealed: int
    reveal_type(x.name)  # revealed: str
```

## Construction syntax

### Value-form construction infers field types from the values

```by
a = (name="asdf", age=20)
reveal_type(a.name)  # revealed: "asdf"
reveal_type(a.age)  # revealed: 20
```

### Mixed positional and named value fields

```by
a = (1, name="a")
reveal_type(a.arg0)  # revealed: 1
reveal_type(a.name)  # revealed: "a"
```

## Assignability

### Plain tuple literal assigns to anonymous named tuple parameter

```by
def f(x: (name: str, age: int)) -> None: ...

f(("asdf", 1))
```

### Plain tuple literal returned from anonymous-named-tuple function

```by
def make() -> (name: str, age: int):
    return ("asdf", 1)
```

### Plain tuple literal assigns through an alias

The coercion follows what the annotation means, not how it is spelled, so an alias to an anonymous
named tuple accepts a plain tuple exactly as the written-out shape does — and the transpiler wraps
the literal at the same three sites either way.

```by
P = (name: str, age: int)

a: P = ("asdf", 1)
reveal_type(a)  # revealed: (name: str, age: int)
reveal_type(a.name)  # revealed: str
```

### Plain tuple literal assigns through a PEP 695 alias

```by
type P = (name: str, age: int)


def make() -> P:
    return ("asdf", 1)


a: P = ("asdf", 1)
b: P | None = ("asdf", 1)
reveal_type(a.age)  # revealed: int
```

### An alias still rejects a mismatched literal

```by
type P = (name: str, age: int)

# error: [invalid-assignment]
a: P = ("asdf", "not an int")
```

### Positional target field accepts named source field of compatible type

```by
a = (name = "asdf", age = 1)
b: (str, age: int) = a
```

### Named target field accepts positional source field of compatible type

```by
a = ("asdf", age = 1)
b: (name: str, age: int) = a
```

### Mismatched field name between named source and named target

```by
a = (other = "asdf", age = 1)
# error: [invalid-assignment]
b: (name: str, age: int) = a
```

### Mismatched field type

```by
a = (name = 1, age = 1)
# error: [invalid-assignment]
b: (name: str, age: int) = a
```

### Mismatched field count

```by
a = (name = "asdf",)
# error: [invalid-assignment]
b: (name: str, age: int) = a
```

### Diagnostic renders anonymous named tuple with surface syntax

```by
a = (name = 1, age = 1)
# error: [invalid-assignment] "Object of type `(name: 1, age: 1)` is not assignable to `(name: str, age: int)`"
b: (name: str, age: int) = a
```

## Inherited tuple members

### The tuple base keeps its element types

The synthesized class inherits from the *specialized* `tuple[int, int]`, so a member reached through
the base is specialized too — the same answer a written `NamedTuple` subclass gives.

```by
from typing import NamedTuple

class P(NamedTuple):
    x: int
    y: int

def f(a: P, b: (x: int, y: int)) -> None:
    reveal_type(a.__iter__)  # revealed: bound method P.__iter__() -> Iterator[int]
    reveal_type(b.__iter__)  # revealed: bound method (x: int, y: int).__iter__() -> Iterator[int]
    reveal_type(list(a))  # revealed: final list[int]
    reveal_type(list(b))  # revealed: final list[int]
    reveal_type(b[0])  # revealed: int
    # the concatenation widens for a written `NamedTuple` too, so the two forms still agree
    reveal_type(a + (1,))  # revealed: (*: int)
    reveal_type(b + (1,))  # revealed: (*: int)
```

### A mixed-type shape unions its element types

```by
def f(b: (name: str, age: int)) -> None:
    reveal_type(b.__iter__)  # revealed: bound method (name: str, age: int).__iter__() -> Iterator[str | int]
    reveal_type(list(b))  # revealed: final list[str | int]
```

## Structural identity

### Two identical shapes share a type

```by
a: (name: str, age: int) = ("a", 0)
b: (name: str, age: int) = ("b", 1)
b = a
```
