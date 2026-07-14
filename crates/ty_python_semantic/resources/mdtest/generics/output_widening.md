# Output-position widening for invariant containers

An invariant generic container cannot be assigned to a wider specialization — `list[int]` is not a
`list[int | None]`, since a holder of the wider type could insert a `None` the original never
expected. But a method that returns a *fresh* container of the same class (`copy`, the set algebra,
...) hands back a brand new object the caller solely owns, so widening its element type at the call
site is sound.

basedpython encodes this in the typeshed stubs (see the `output-widening` patch): each such method
gains a `Never`-defaulted type parameter unioned into every invariant position of its return. With
an expected type the parameter solves to the widening; with no expected type it defaults to `Never`
and `T | Never` collapses back to `T`, so ordinary inference is unchanged.

Only methods reached by an ordinary call are widened — a call already threads the caller's expected
type into inference. Operator dunders (`+`, `*`, `[]`, `&`, `|`, ...) are left alone: making their
returns widen would require bidirectional inference on every binary op and subscript, which is far
too expensive on real code.

```toml
[environment]
python-version = "3.13"
```

## `list`

```py
def f(a: list[int]):
    widened: list[int | None] = a.copy()
    reveal_type(a.copy())  # revealed: list[int]

    # widening only ever *adds* to the union; it cannot replace the element type
    # error: [invalid-assignment]
    bad: list[str] = a.copy()
```

## `set`

Every fresh-set method widens, including the ones that already infer an element from their argument:

```py
def f(s: set[int]):
    via_copy: set[int | None] = s.copy()
    via_difference: set[int | None] = s.difference({1})
    via_intersection: set[int | None] = s.intersection({1})
    via_union: set[int | str | None] = s.union(["x"])
    via_symmetric: set[int | str | None] = s.symmetric_difference(["x"])

    reveal_type(s.copy())  # revealed: set[int]
    reveal_type(s.union(["x"]))  # revealed: set[int | str]
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

## Operators are not widened

The result of an operator keeps its natural type; assigning it to a wider invariant specialization
is still rejected (this behaves exactly as it did before the feature):

```py
def f(a: list[int]):
    # error: [invalid-assignment]
    bad: list[int | None] = a + a
```

Heterogeneous operators keep resolving to their natural result, unchanged:

```py
class A: ...
class B: ...

def g(x: list[A], y: list[B]):
    z: list[A | B] = x + y
    reveal_type(x + y)  # revealed: list[B | A]
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
