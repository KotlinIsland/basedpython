# basedpython: `literal` and `final` use-site type modifiers

`literal T` and `final T` are keywords written in front of a type expression that narrow which
values the annotated place accepts:

- `literal T` accepts a value whose type is a *literal type*
- `final T` accepts a value whose runtime class is exactly `T`'s, so a proper subtype is rejected

Both are compile-time-only: the keyword is erased in the lowered Python.

```toml
[environment]
python-version = "3.12"
```

## `literal str` is `LiteralString`

```by
a: literal str = "asdf"
reveal_type(a)  # revealed: "asdf"

def s() -> str:
    return ""

# error: [invalid-assignment] "Object of type `str` is not assignable to `LiteralString`"
b: literal str = s()
```

The reduction is to the very same type, so the two spellings are interchangeable:

```by
from typing import LiteralString

def f(x: literal str) -> LiteralString:
    return x

def g(x: LiteralString) -> literal str:
    return x
```

## `literal` on other literal types

```by
a: literal int = 1
b: literal bool = True
c: literal bytes = b"x"

def i() -> int:
    return 1

# error: [invalid-assignment] "Object of type `int` is not assignable to `literal int`"
d: literal int = i()
```

A folded literal expression is still a literal:

```by
a: literal int = 1 + 1
reveal_type(a)  # revealed: 2
```

## `literal` on an enum member

```by
import enum

class Color(enum.Enum):
    RED = 1
    GREEN = 2

a: literal Color = Color.RED

def c() -> Color:
    return Color.RED

# error: [invalid-assignment]
b: literal Color = c()
```

## `literal` on a container

A specialized generic is literal when every type argument is. `[]` infers `list[Never]`, whose only
inhabitant is the empty list display, so it fits; a `list[int]` does not.

```by
a: literal list[*] = []

# error: [invalid-assignment] "Object of type `list[int]` is not assignable to `literal list[*]`"
b: literal list[*] = [1]

def xs() -> list[int]:
    return []

# error: [invalid-assignment]
c: literal list[*] = xs()
```

## `final` rejects a proper subtype

```by
a: final int = 1

# error: [invalid-assignment] "Object of type `True` is not assignable to `final int`"
b: final int = True
```

## `final` on a constructor call

```by
class A: ...

class B(A): ...

a: final A = A()

# error: [invalid-assignment] "Object of type `final B` is not assignable to `final A`"
b: final A = B()
```

## a constructor call is inferred `final`

A call that names the class it builds produces a value whose runtime class is exactly that class.
Like `Literal[1]` for `1`, the extra precision is dropped wherever a declaration is inferred from
the value.

```by
class A: ...

class B(A): ...

def f(make: type[A]):
    reveal_type(A())  # revealed: final A
    a = A()
    reveal_type(a)  # revealed: final A
    # a `type[A]` variable may hold a subclass, so its call is only an `A`
    reveal_type(make())  # revealed: A
    # a list display widens its elements, so the modifier does not leak into a
    # type argument
    reveal_type([A()])  # revealed: list[A]
```

## a generic solve reads through the modifier

The restriction constrains which values fit, not what shape they have, so a constructed value still
matches a structural formal.

```by
def head[T](xs: list[T]) -> T:
    raise NotImplementedError

def f():
    reveal_type(head(list[int]()))  # revealed: int
    xs = list[int]()
    reveal_type(head(xs))  # revealed: int
```

## an inferred declaration widens the modifier away

```by
class A: ...

class B(A): ...

class C:
    x = A()

def g(c: C):
    reveal_type(c.x)  # revealed: A
    c.x = B()
```

## `final T` is disjoint from a type no exactly-`T` value can have

A proper subclass of `A` is never exactly an `A`, and neither is an unrelated class, so both narrow
away entirely.

```by
class A: ...

class B(A): ...

def f(a: final A):
    if isinstance(a, B):
        reveal_type(a)  # revealed: Never
    if isinstance(a, str):
        reveal_type(a)  # revealed: Never
```

## a value `final T` admits keeps overlapping it

`1` is exactly an `int`, so `final int` still meets `Literal[1]`.

```by
def f(n: final int):
    if isinstance(n, bool):
        reveal_type(n)  # revealed: Never
    if n == 1:
        reveal_type(n)  # revealed: final int
```

## `final` on an already-`@final` class adds nothing

```by
from typing import final as final_decorator

@final_decorator
class A: ...

def f(x: final A) -> None:
    reveal_type(x)  # revealed: A
```

## a modifier binds tighter than a union

`literal str | None` is `(literal str) | None`, so the `None` arm is unrestricted.

```by
a: literal str | None = None
b: literal str | None = "x"

def s() -> str:
    return ""

# error: [invalid-assignment]
c: literal str | None = s()
```

## modifiers in parameter and return position

```by
def f(x: literal int) -> None: ...

f(1)

def i() -> int:
    return 1

# error: [invalid-argument-type]
f(i())

def g() -> final int:
    return 1

reveal_type(g())  # revealed: final int
```

## a restricted value is still an ordinary value

In source position the modifier says nothing new, so members, operators and iteration all work
through the type it wraps.

```by
def f(a: literal str, b: final int, c: literal list[*]) -> None:
    reveal_type(a.upper())  # revealed: LiteralString
    reveal_type(b + 1)  # revealed: int
    reveal_type(len(c))  # revealed: int
    d: str = a
    e: int = b
```

## a modifier is only a modifier when a name follows it

```by
class literal: ...

def f(a: literal) -> None:
    reveal_type(a)  # revealed: literal
```

## the modifiers are basedpython-only

<!-- snapshot-diagnostics -->

```py
# error: [invalid-syntax]
a: literal str = "x"
```
