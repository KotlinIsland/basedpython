# basedpython: `&` intersection operator

basedpython exposes intersections via the `&` operator in type positions. ty narrows the operand
types intersectionally; the same semantics as `ty_extensions.Intersection[A, B]` but expressed as
surface syntax.

```toml
[environment]
python-version = "3.12"
```

## simple two-type intersection

```by
class P: ...
class Q: ...

def f(x: P & Q) -> None:
    reveal_type(x)  # revealed: P & Q
```

## intersection of attribute presence

```by
class HasA:
    a: int

class HasB:
    b: str

def f(x: HasA & HasB) -> tuple[int, str]:
    reveal_type(x.a)  # revealed: int
    reveal_type(x.b)  # revealed: str
    return (x.a, x.b)
```

## intersection inside a generic

```by
class A: ...
class B: ...

def f(items: list[A & B]) -> None:
    if items:
        reveal_type(items[0])  # revealed: A & B
```

## three-arm intersection

```by
class A: ...
class B: ...
class C: ...

def f(x: A & B & C) -> None:
    reveal_type(x)  # revealed: A & B & C
```

## intersection in a type spelled through a call

A few typing constructs name a type in a call argument rather than after a `:` — `NewType` names its
base, `TypeVar` names its bound and its constraints, and the functional `NamedTuple` and `TypedDict`
name their field types inside a literal. Those arguments are type expressions, so `&` means the same
thing in one as it does in an annotation, and the transpiler has to lower it there too: an `A & B`
that survived into the emitted python would be a runtime `A.__and__(B)`.

```by
import typing

class A: ...

class B: ...

T = typing.TypeVar("T", bound=A & B)
U = typing.TypeVar("U", A & B, int)
Pair = typing.NamedTuple("Pair", [("left", A & B), ("right", int)])
Row = typing.TypedDict("Row", {"cell": A & B})

def f(x: Pair, y: Row) -> None:
    reveal_type(x.left)  # revealed: A & B
    reveal_type(y["cell"])  # revealed: A & B
```

## an ordinary call is not one of those constructs

Only a call that resolves to one of those constructs reads its arguments as types. Anywhere else
`and` is python's boolean operator, and the value it produces is the one the call receives.

```by
import typing

class A: ...

class B(A): ...

Both = typing.NewType("Both", B)

def pick(value: object) -> object:
    return value

chosen = pick(A and B)
reveal_type(chosen)  # revealed: object
```
