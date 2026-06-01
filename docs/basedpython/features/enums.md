# based enums

basedpython supports algebraic sum types — "based enums" — modeled after
rust and swift enums but with pythonic surface syntax. variants can carry
typed payloads, support pattern matching, and integrate with ty's
exhaustiveness checking. methods, classmethods, properties, and generics
live directly on the enum body. type-directed emit lowers the surface form
to a sealed dataclass hierarchy at runtime

## goals

- sum types (tagged unions) like rust/swift
- pythonic body: `def` for methods, `[T]` for generics, no foreign keywords
- type-directed emit: clean surface, valid python 3.10+ runtime
- no syntax cages: variants can be payload-less or carry typed fields with
    defaults; methods, properties, classmethods, and staticmethods all live on
    the enum; recursive and generic forms supported

## surface syntax

variants are declared with the `case` keyword (as in swift/scala); one `case`
line may declare several comma-separated variants. anything that isn't a
`case` line is an ordinary class-body statement

```by
enum class Shape:
    case Circle(radius: float)
    case Rectangle(width: float, height: float)
    case Point
    case Polygon(sides: list[Point], closed: bool = True)

    def area(self) -> float:
        match self:
            case Shape.Circle(r): 3.14 * r * r
            case Shape.Rectangle(w, h): w * h
            case Shape.Point: 0.0
            case Shape.Polygon(sides, closed=True): shoelace(sides)
            case Shape.Polygon(): 0.0

    @classmethod
    def unit_circle(cls) -> Shape:
        return Shape.Circle(1.0)
```

three variant forms, mix freely within one enum (and within one `case` line):

- **unit** — `case Point` — a singleton *value* (reached as `Shape.Point`, matched `case Shape.Point:`), no payload
- **positional** — `case Circle(float)` — positional construct, anonymous fields
- **named** — `case Circle(radius: float)` — named fields available for
    pattern matching, attribute access, and keyword construction; fields may
    carry defaults (`case Polygon(sides: int, closed: bool = True)`), defaulted
    fields last

the compact comma form reads best for unit variants:

```by
enum class Color:
    case Red, Green, Blue
```

generic and recursive forms:

```by
enum class Tree[T]:
    case Leaf
    case Node(T, Tree[T], Tree[T])

    def depth(self) -> int:
        match self:
            case Tree.Leaf: 0
            case Tree.Node(_, l, r): 1 + max(l.depth(), r.depth())

enum class Result[T, E]:
    case Ok(T)
    case Err(E)
```

bounds use the same syntax as based generics elsewhere:
`enum class E[T: Hashable]`, `enum class E[T: constraints (int, str)]`

## variant access

variants lower to **subclasses** of the enum attached as class attributes, so
they are reached qualified through the enum name — `Shape.Circle(2.0)`,
`Shape.Point` — everywhere: inside the enum body, in pattern contexts
(`case Shape.Circle(r):`), and at module scope. variant constructors are real
classes at runtime, so `x is Shape.Circle` works (recall `is` is basedpython's
[`isinstance`](identity-swap.md); use `type(x) === Shape.Circle` for an
exact-class check). because variants are qualified, the same variant name may
appear in two different enums (`A.Same` vs `B.Same`) without collision.

## methods and other members

the enum body is a regular suite. anything that can appear in a python class
body can appear in an enum body:

- `def` methods (dispatched on the union; usually implemented via `match self`)
- `@classmethod`, `@staticmethod`, `@property`
- nested types
- class-level constants

methods live in the enum class body. variant-specific methods can be
declared by narrowing the receiver type: `def f(self: Shape.Circle) -> float`

## pattern matching

based enums plug directly into python's `match` statement. variant patterns
use the variant constructor form:

```by
match shape:
    case Shape.Circle(r): ...
    case Shape.Rectangle(w, h): ...
    case Shape.Point: ...
    case Shape.Polygon(sides=s, closed=True): ...
    case Shape.Polygon(sides=s, closed=c): ...
```

ty checks **exhaustiveness**: a `match` over a based enum that fails to
cover every variant produces a diagnostic. wildcard `case _:` opts out of
exhaustiveness for that match. exhaustiveness is enforced in both statement
and expression positions

## expression-form match

`match` can appear in expression position (e.g. the body of a one-line
method, the rhs of `=`):

```by
def area(self) -> float:
    match self:
        case Shape.Circle(r): 3.14 * r * r
        case Shape.Rectangle(w, h): w * h
        case Shape.Point: 0.0
```

every arm's body must produce a value of the common type. ty infers the
union and applies exhaustiveness as usual

## variant as type

a single variant name is usable as a type. ty narrows the receiver:

```by
def double_radius(c: Shape.Circle) -> Shape.Circle:
    return Shape.Circle(c.radius * 2)
```

assignability follows the obvious rule: `Shape.Circle` is a subtype of
`Shape`, but `Shape` is not a subtype of `Shape.Circle`

## derived behaviour

payload variants lower to frozen dataclasses, so they come with `__eq__`,
`__hash__`, `__repr__`, and `__match_args__` derived from their fields.
equality is structural (two `Circle(2.0)` values compare equal), values are
hashable (usable as dict keys / in sets) when every payload field is hashable,
and `repr` reads as the construction form (`Shape.Circle(radius=2.0)`)

see also: [destructuring with `if let`](if-let.md) (planned)

## prelude enums

based prelude ships `Option[T]` and `Result[T, E]` as based enums. user
code gets them without an import:

```by
def find(xs: list[int], target: int) -> Option[int]:
    for i, x in enumerate(xs):
        if x == target:
            return Some(i)
    return None
```

`None` here is the based enum unit variant, not python's `None`. at runtime
the prelude maps based `None` to a sentinel that is `==`-equivalent to
python `None` for ergonomic interop (open question, see below)

## transpiler output

an enum whose variants are **all unit** (no payloads) lowers to an idiomatic
`enum.Enum` with `auto()` members — `enum class Color: case Red, Green` becomes
`class Color(Enum): Red = auto(); Green = auto()`. this is the form the reverse
transform recognises.

any other enum (one or more payload-carrying variants) lowers to a sealed
hierarchy: the enum class holds the shared members, and each variant becomes a
module-level **subclass** of the enum attached back as `Shape.Circle` — payload
variants as frozen dataclasses, unit variants as singleton *values*. subclassing
is what makes methods declared on the enum body dispatch on the variants.
`Shape.Circle(2.0)` constructs; the enum name itself is the type (the type
checker treats it as the union of its variants). the output is prefixed with
`from __future__ import annotations` so mutually-recursive references
(recursive enums → themselves) resolve lazily:

```python
from __future__ import annotations
from dataclasses import dataclass
from typing import final

class Shape:
    def area(self) -> float: ...  # methods live in the enum body

@final
@dataclass(frozen=True, slots=True)
class _Shape_Circle(Shape):
    radius: float
_Shape_Circle.__name__ = "Circle"
_Shape_Circle.__qualname__ = "Shape.Circle"
Shape.Circle = _Shape_Circle

class _Shape_Point(Shape):
    __slots__ = ()
    def __repr__(self): return "Point"
_Shape_Point.__name__ = "Point"
_Shape_Point.__qualname__ = "Shape.Point"
Shape.Point = _Shape_Point()  # the variant is the singleton value, not the class
```

`__match_args__` comes from dataclass for payload variants. generic enums lower
to a `class Shape[T]:` (PEP 695) with the variant subclasses parametrised the
same way.
