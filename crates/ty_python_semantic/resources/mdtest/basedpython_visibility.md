# basedpython: visibility modifiers

`private` and `export`/`public` are transpile-time visibility modifiers: `export`/`public` add the
symbol to the module's generated `__all__`, and `private` renames it with an underscore prefix (or,
inside a class body, name-mangles it with `__`). they carry no type-level effect — the decorated
class or function keeps its ordinary type rather than being erased to `Unknown`.

a dunder is the exception: python looks one up by its exact name, and mangles only names with at
most one trailing underscore, so renaming would change what the method *is* rather than who can
reach it. `private` on one is therefore reported as having no effect — except on `__init__`, the one
dunder where it says something, which is checked at the construction site instead.

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

## `private` on a name it cannot hide

`private` hides a member by renaming it so python's name-mangling applies. a name python looks up
verbatim is left as written, so the modifier would silently do nothing, and it is reported instead.

```by
class Point:
    private def __repr__(self) -> str:  # error: [ineffective-private]
        return "Point()"

    # `__` + `_` is `___`, which python also looks up verbatim
    private def _(self): ...  # error: [ineffective-private]
```

a name with at most one trailing underscore is hidden as usual, and `__init__` is the one dunder
`private` does say something about.

```by
class Id:
    private init()

    private def helper(self): ...
    private def unhide_(self): ...
```

## a `private` constructor may only be called by its own class

name-mangling is how `private` hides a class member, but python calls a constructor by its exact
name, so there is no spelling that would hide `__init__` and still leave it a constructor. a
`private init` is enforced at the construction site instead: the class's own body may construct it,
and nothing else may. that is what lets a class hand out its instances through a factory of its own.

```by
class Id:
    private init(let raw: str)

    @classmethod
    def parse(cls, text: str) -> Id:
        return Id(text.strip())

reveal_type(Id.parse(" a ").raw)  # revealed: str

made = Id("a")  # error: [private-constructor]
```

## the diagnostic points back at the declaration

the class that drew the boundary is named at the construction site and annotated where it declared
the constructor.

```by
class Id:
    private init(let raw: str)

made = Id("a")  # snapshot
```

```snapshot
error[private-constructor]: Cannot construct `Id`: its constructor is private
 --> src/mdtest_snippet.by:4:8
  |
4 | made = Id("a")  # snapshot
  |        ^^^^^^^
info: Only code inside `Id` may construct it
 --> src/mdtest_snippet.by:2:13
  |
2 |     private init(let raw: str)
  |             ---- `Id`'s constructor declared private here
```

## every scope inside the class is the class's own code

a method's local function and a nested class are written inside the body, so they construct it too.

```by
class A:
    private init()

    def clone(self):
        def build():
            return A()

        return build()

    class Inner:
        @staticmethod
        def make():
            return A()

reveal_type(A.Inner.make().clone())  # revealed: final A
```

## a subclass is outside a `private` constructor's class

a subclass may not construct its base, and — because it inherits the private constructor — may not
be constructed itself.

```by
class Base:
    private init()

class Derived(Base): ...

base = Base()  # error: [private-constructor]
derived = Derived()  # error: [private-constructor]
```

## the boundary is the declaring class, not the declaring module

another module may hold the class, name it, and pass it around. what it may not do is call it.

`ids.by`:

```by
class Id:
    private init(let raw: str)

    @classmethod
    def parse(cls, text: str) -> Id:
        return Id(text.strip())
```

`callers.by`:

```by
from ids import Id

parsed = Id.parse(" a ")
reveal_type(parsed.raw)  # revealed: str

made = Id("a")  # error: [private-constructor]

# naming the class does not launder it: the value is still the class itself
alias = Id
aliased = alias("a")  # error: [private-constructor]
```

## a subclass reusing its base's name is still told whose constructor it is

the two classes are told apart by identity rather than by name, so the message says the constructor
was inherited even where both classes are called `Id`.

`base_id.by`:

```by
class Id:
    private init()
```

`shadowing.by`:

```by
import base_id

class Id(base_id.Id): ...

# error: [private-constructor] "Cannot construct `Id`: it inherits `Id`'s private constructor"
made = Id()
```

## `type[A]` is not refused

a `type[A]` may hold a subclass, and a subclass is free to declare a constructor of its own — the
same reason `type[SomeProtocol]` may be called where the protocol class itself may not. so the
guarantee a `private init` gives is over the class's own name, not over every route to a class
object.

```by
class Id:
    private init()

def build(cls: type[Id]) -> Id:
    return cls()
```

## declaring a constructor makes a subclass constructible again

```by
class Base:
    private init()

class Derived(Base):
    init()

derived = Derived()
reveal_type(derived)  # revealed: final Derived
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
