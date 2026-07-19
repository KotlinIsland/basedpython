# basedpython: `final` annotation modifier

basedpython spells an explicit `Final` declaration with the `final` modifier before the name:
`final x: T` is `x: Final[T]`. Unlike `let` (which is read-only only at module scope), `final` is
`Final` in every scope.

```toml
[environment]
python-version = "3.12"
```

## `final` is `Final` at module scope

```by
final MOD: int = 5
reveal_type(MOD)  # revealed: 5

MOD = 6  # error: [invalid-assignment]
```

## `final` is `Final` inside a class (unlike `let`)

```by
class C:
    final tag: str = "c"

reveal_type(C.tag)  # revealed: str
C.tag = "x"  # error: [invalid-assignment]

c = C()
c.tag = "y"  # error: [invalid-assignment]
```

## valueless `final` in a stub

`m.byi`:

```byi
final CONST: int

class C:
    final attr: str
```

```by
from m import CONST, C

reveal_type(CONST)  # revealed: int
CONST = 3  # error: [invalid-assignment]

reveal_type(C().attr)  # revealed: str
C().attr = "z"  # error: [invalid-assignment]
```

## complex type expressions

the declared type is shown valueless in a stub; with a value the `Final` narrows to it.

`n.byi`:

```byi
final PAIR: tuple[int, str]
final MAYBE: int | None
```

```by
from n import PAIR, MAYBE

reveal_type(PAIR)  # revealed: (int, str)
reveal_type(MAYBE)  # revealed: int | None
```

## `final` survives in a modifier chain

`final` combined with another modifier still applies `Final` (the other modifier is stripped, but
the qualifier is kept):

```by
class C:
    final override tag: str = "c"

reveal_type(C.tag)  # revealed: str
C.tag = "x"  # error: [invalid-assignment]
```

## a `let` attribute is read-only but may be overridden; `final` may not

A valueless `let x: T` is `Final` (read-only), but it models a read-only *property*, so a subclass
is still allowed to override it. An explicit `final` forbids the override. This is what lets the
`property`-to-`let` typeshed rewrite keep overridable properties overridable.

`base.byi`:

```byi
class Base:
    let name: str
    final tag: str
```

```by
from base import Base

class Sub(Base):
    name: str  # a read-only `let` (property-like) may be overridden
    tag: str  # error: [override-of-final-variable]
```

## a bare, valueless `let x`

A `let x` with neither a type nor an initializer declares an uninitialized `Final`: the type is
inferred from the single later assignment, and a second assignment is rejected.

```by
let a
a = 1
reveal_type(a)  # revealed: 1

a = 2  # error: [invalid-assignment]
```
