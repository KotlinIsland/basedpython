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
reveal_type(b)  # revealed: Box
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
