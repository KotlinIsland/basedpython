# basedpython: a tuple type as a class base

`(*: T)` is basedpython for `tuple[T, ...]`, and `(A, B)` is basedpython for `tuple[A, B]`. Both are
type-only forms — no runtime value is ever spelled that way — so in a class base list they name the
tuple type the class inherits from, exactly as the transpiler lowers them.

```toml
[environment]
python-version = "3.12"
```

## a variadic tuple base is a `tuple` subclass

```by
class Row((*: int)):
    pass

def f(row: Row):
    reveal_type(row[0])  # revealed: int
    for cell in row:
        reveal_type(cell)  # revealed: int
```

## a variadic tuple base does not make the class assignable to everything

The base has to resolve. When it does not, the class carries `Unknown` in its MRO and silently
satisfies every annotation.

```by
class Row((*: int)):
    pass

def f(row: Row):
    # error: [invalid-assignment] "Object of type `Row` is not assignable to `int`"
    x: int = row
    y: (*: int) = row
```

## `sys.flags` is not an `int`

`sys.flags` is declared in the typeshed with exactly this base, so it is the case that first showed
the hole.

```py
import sys

# error: [invalid-assignment] "Object of type `_flags` is not assignable to `int`"
x: int = sys.flags
reveal_type(sys.flags.debug)  # revealed: int
```

## a variadic tuple base composes with a nominal base

```by
class Mixin:
    def describe(self) -> str:
        return "row"

class Row(Mixin, (*: str)):
    pass

def f(row: Row):
    reveal_type(row.describe())  # revealed: str
    reveal_type(row[0])  # revealed: str
```
