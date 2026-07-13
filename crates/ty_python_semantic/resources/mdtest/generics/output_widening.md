# Output-position widening for invariant containers

An invariant generic container cannot be assigned to a wider specialization — `list[int]` is not a
`list[int | None]`, since a holder of the wider type could insert a `None` the original never
expected. But a method that returns a *fresh* container of the same class (`copy`, `+`, `*`, the set
algebra, ...) hands back a brand new object the caller solely owns, so widening its element type at
the call site is sound.

basedpython encodes this in the typeshed stubs (see the `output-widening` patch): each such method
gains a `Never`-defaulted type parameter unioned into every invariant position of its return. With
an expected type it solves to the widening; without one it defaults to `Never` and `T | Never`
collapses back to `T`, so ordinary inference is unchanged.

```toml
[environment]
python-version = "3.13"
```

## `list`

Widening flows through both plain method calls and operator syntax:

```py
def f(a: list[int]):
    via_copy: list[int | None] = a.copy()
    via_add: list[int | None] = a + a
    via_mul: list[int | None] = a * 3
    via_slice: list[int | None] = a[0:2]
```

With no expected type the widening parameter defaults to `Never`, so inference stays precise:

```py
def g(a: list[int]):
    reveal_type(a.copy())  # revealed: list[int]
    reveal_type(a + a)  # revealed: list[int]
    reveal_type(a * 3)  # revealed: list[int]
    reveal_type(a[0:2])  # revealed: list[int]
```

Widening only ever *adds* to the union; it cannot replace the element type:

```py
def h(a: list[int]):
    # error: [invalid-assignment]
    bad: list[str] = a.copy()
```

## `set`

```py
def f(s: set[int]):
    via_copy: set[int | None] = s.copy()
    via_and: set[int | None] = s & {1}
    via_or: set[int | str | None] = s | {"x"}
    via_union: set[int | str | None] = s.union(["x"])
    via_difference: set[int | None] = s.difference({1})

    reveal_type(s.copy())  # revealed: set[int]
    reveal_type(s & {1})  # revealed: set[int]
```

## `dict`

Each invariant position is widened independently:

```py
def f(m: dict[str, int]):
    both: dict[str | None, int | None] = m.copy()
    key_only: dict[str | None, int] = m.copy()
    value_only: dict[str, int | None] = m.copy()

    reveal_type(m.copy())  # revealed: dict[str, int]
```

## Covariant containers are unaffected

`frozenset` is immutable and therefore covariant, so its copies already widen without any special
machinery — and the patch correctly leaves it alone:

```py
def f(s: frozenset[int]):
    widened: frozenset[int | None] = s.copy()
    reveal_type(s.copy())  # revealed: frozenset[int]
```

## User-defined invariant classes

The same pattern applies to any invariant class. `Widen` defaults to `Never`, so `T | Widen`
collapses to `T` when the caller gives no expected type. (The return annotation is quoted because
basedpython evaluates annotations eagerly, and the class is not yet bound inside its own body.)

```py
from typing import Never

class Box[T]:
    def push(self, value: T) -> None: ...
    def copy[Widen = Never](self) -> "Box[T | Widen]":
        raise NotImplementedError

def f(b: Box[int]):
    widened: Box[int | None] = b.copy()
    reveal_type(b.copy())  # revealed: Box[int]
```

Without the widening parameter, an invariant box cannot be widened at all:

```py
class Plain[T]:
    def push(self, value: T) -> None: ...
    def copy(self) -> "Plain[T]":
        raise NotImplementedError

def f(p: Plain[int]):
    # error: [invalid-assignment]
    widened: Plain[int | None] = p.copy()
```
