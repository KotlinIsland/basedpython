# `bool` implicitly used as `int`

`bool` is a subclass of `int`, and `True` and `False` genuinely are `1` and `0` — nothing is
converted. `bool-as-int` reports a boolean that satisfies a numeric target only by way of that
subclass relation, where a boolean written there by mistake type-checks like one that was meant.

```toml
[environment]
python-version = "3.12"
```

## an annotated assignment

```py
# error: [bool-as-int] "`Literal[True]` is implicitly used as `int`"
a: int = True
```

## a variable of type `bool`

the value need not be written as a literal — any expression whose type is a boolean counts.

```py
def flag() -> bool:
    return False

# error: [bool-as-int] "`bool` is implicitly used as `int`"
a: int = flag()

b = False
# error: [bool-as-int] "`Literal[False]` is implicitly used as `int`"
c: int = b
```

## assignment to a name declared elsewhere

```py
a: int
# error: [bool-as-int] "`Literal[True]` is implicitly used as `int`"
a = True
```

## a call argument

```py
def take(n: int): ...

# error: [bool-as-int] "`Literal[True]` is implicitly used as `int`"
take(True)
```

## a keyword argument

```py
def take(*, n: int): ...

# error: [bool-as-int] "`Literal[True]` is implicitly used as `int`"
take(n=True)
```

## a constructor argument

the receiver's synthetic `self` is not an argument written at the call site, so the offset does not
shift which parameter a boolean is checked against.

```py
class C:
    def __init__(self, n: int): ...

# error: [bool-as-int] "`Literal[True]` is implicitly used as `int`"
C(True)
```

## a method argument

```py
class C:
    def m(self, n: int): ...

# error: [bool-as-int] "`Literal[True]` is implicitly used as `int`"
C().m(True)
```

## a return statement

```py
def f() -> int:
    # error: [bool-as-int] "`Literal[True]` is implicitly used as `int`"
    return True
```

## an attribute assignment

```py
class C:
    n: int

    def __init__(self):
        # error: [bool-as-int] "`Literal[True]` is implicitly used as `int`"
        self.n = True

# error: [bool-as-int] "`Literal[True]` is implicitly used as `int`"
C().n = True
```

## a parameter default

```py
# error: [bool-as-int] "`Literal[True]` is implicitly used as `int`"
def f(n: int = True): ...
```

## `float` and `complex` are reached through `int` too

an `int` annotation is implicitly widened to accept `float`, and a `bool` rides that same edge.

```py
# error: [bool-as-int] "`Literal[True]` is implicitly used as `int | float`"
a: float = True

# error: [bool-as-int] "`Literal[True]` is implicitly used as `int | float | complex`"
b: complex = True
```

## a union that mentions `int`

```py
# error: [bool-as-int] "`Literal[True]` is implicitly used as `int | str`"
a: int | str = True

# error: [bool-as-int] "`Literal[True]` is implicitly used as `int | None`"
b: int | None = True
```

## a `bool` target is not flagged

```py
a: bool = True
b: bool = False

def take(flag: bool): ...

take(True)

def f() -> bool:
    return True
```

## `object` is not flagged

`object` is not a numeric target — nothing is being read as a number there.

```py
a: object = True

def take(o: object): ...

take(True)
print(True)
```

## a gradual target is not flagged

a dynamic component can stand for `bool` in its own right, so the subclass relation is not what let
the value through.

```py
from typing import Any

a: Any = True

def take(x: Any): ...

take(True)

def untyped(x): ...

untyped(True)
```

## an unsolved type variable is not flagged

the type variable solves to the boolean itself; nothing widens it to a number.

```py
def identity[T](x: T) -> T:
    return x

identity(True)

def numeric[N: int](x: N) -> N:
    return x

numeric(True)
```

## an explicit conversion is not flagged

`int(...)` is the intended way to say the numeric value is what was meant.

```py
a: int = int(True)

def take(n: int): ...

take(int(True))

def f() -> int:
    return int(True)
```

## arithmetic on booleans is not flagged

`True + 1` forwards `True` to `int.__add__` as the receiver, which is a use of the boolean as a
boolean, not a value written into a numeric slot.

```py
a = True + 1
b = True == 1
c = abs(True)
d = sum([True, False])
e = str(True)
```

## a container of booleans is not flagged

only a boolean flowing directly into a numeric target is reported; the element types of a collection
are checked by ordinary assignability instead.

```py
a: list[bool] = [True, False]
b: tuple[bool, ...] = (True,)
c: dict[str, bool] = {"k": True}
```

## `int | bool` collapses to `int` before the check runs

a union of a class and its subclass simplifies to the supertype, so the `bool` arm is gone by the
time any check sees the annotation. `int | bool` really is `int`, and it is reported as such.

```py
def take(n: int | bool) -> None: ...

reveal_type(take)  # revealed: def take(n: int)

# error: [bool-as-int] "`Literal[True]` is implicitly used as `int`"
a: int | bool = True
```

## suppression

```py
a: int = True  # ty: ignore[bool-as-int]
```

## a basedpython file

the lint is not gated on the source language. a `.by` file spells a boolean literal type as `True`
rather than `Literal[True]`, so the message follows the file it is reported in.

```by
def take(n: int) -> None:
    pass

# error: [bool-as-int] "`True` is implicitly used as `int`"
a: int = True

# error: [bool-as-int] "`True` is implicitly used as `int`"
take(True)

def f() -> int:
    # error: [bool-as-int] "`True` is implicitly used as `int`"
    return True

b: bool = True
c = True + 1
```
