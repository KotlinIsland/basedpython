# basedpython: visibility modifiers

`private` and `export`/`public` are transpile-time visibility modifiers: `export`/`public` add the
symbol to the module's generated `__all__`, and `private` renames it with an underscore prefix (or,
inside a class body, name-mangles it with `__`). they carry no type-level effect — the decorated
class or function keeps its ordinary type rather than being erased to `Unknown`.

## a private class keeps its type

```by
private class Box:
    value: int = 0

b = Box()
reveal_type(b)  # revealed: final Box
reveal_type(b.value)  # revealed: int
```

## an exported function keeps its signature

```by
export def make(n: int) -> int:
    return n * 2

reveal_type(make)  # revealed: def make(n: int) -> int
reveal_type(make(3))  # revealed: int
```

## `open` is a no-op modifier on the type

`open` marks a class freely subclassable (the default in Python); it only suppresses the closed-by-
default checks, so the class type is unchanged.

```by
open class Base:
    tag: str = "b"

reveal_type(Base().tag)  # revealed: str
```

## modifiers compose with each other

a chain of modifiers still resolves to the underlying declaration's type.

```by
private final class Sealed:
    n: int = 1

reveal_type(Sealed().n)  # revealed: int
```

## `private type` aliases bind the unmangled name

the `_` prefix is applied by the lowering; in the type checker the alias binds the name as written.

```by
private type Key = str | int

def lookup(k: Key) -> None: ...

reveal_type(lookup)  # revealed: def lookup(k: Key)
```

## a private alias may be used freely inside its own module

`store.by`:

```by
private type Key = str | int

type Table = dict[Key, int]

def get(t: Table, k: Key) -> int:
    return t[k]
```

## importing a private symbol from another module is an error

`helpers.by`:

```by
private type Key = str | int

private def secret() -> int:
    return 1

private class Hidden: ...

type Open = list[int]
```

`main.by`:

```by
from helpers import Open  # fine
from helpers import Key  # error: [private-import] "`Key` is private to `helpers`"
from helpers import secret  # error: [private-import] "`secret` is private to `helpers`"
from helpers import Hidden  # error: [private-import] "`Hidden` is private to `helpers`"
```

## renaming on import does not launder a private symbol

`helpers2.by`:

```by
private type Key = str | int
```

`main2.by`:

```by
from helpers2 import Key as K  # error: [private-import] "`Key` is private to `helpers2`"
```

## a real `@private` decorator is not the modifier

a decorator written with `@` is an ordinary decorator, so the symbol stays importable.

`deco.by`:

```by
def private[T](f: T) -> T:
    return f

@private
def helper() -> int:
    return 1
```

`main3.by`:

```by
from deco import helper

reveal_type(helper())  # revealed: int
```
