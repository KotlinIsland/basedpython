# basedpython: pydantic models

pydantic support is inherited from upstream ty (model detection, constructor synthesis, strict/lax
inputs — see `external/pydantic.md`). this suite is the basedpython half: models *written in `.by`*
must both check precisely and, once transpiled, behave under the real framework. every `.by` block
here is checker-clean, so the runtime-divergence harness (`crates/ty/tests/mdtest_divergence.rs`)
also transpiles and executes it against an installed pydantic.

```toml
[environment]
python-version = "3.13"
python-platform = "linux"

[project]
dependencies = ["pydantic==2.13.4", "pydantic-settings==2.14.2"]
```

## float and complex fields validate at runtime

basedpython's `float` / `complex` are the int-excluding builtins, lowered to the `JustFloat` /
`JustComplex` `ty_extensions` aliases. that exclusion is static-only: at runtime the aliases *are*
the builtins, so a model that introspects its annotations (pydantic reads them to build its schema)
validates the fields as ordinary `float` / `complex`.

```by
from pydantic import BaseModel

class Item(BaseModel):
    name: str
    price: float
    weight: complex

i = Item(name="x", price=1.5, weight=2j)
reveal_type(i.name)  # revealed: str
reveal_type(i.price)  # revealed: float
assert i.price == 1.5
assert i.model_dump()["price"] == 1.5
```

## all-payload-less based enum as a field type

an enum whose variants are all payload-less lowers to a stdlib `Enum`, which pydantic validates
natively.

```by
from pydantic import BaseModel

enum class Color:
    case Red
    case Green
    case Blue

class Paint(BaseModel):
    name: str
    color: Color

p = Paint(name="sky", color=Color.Blue)
assert p.color == Color.Blue
assert p.model_dump()["color"] == Color.Blue
```

## a payload variant as a field type

a payload-bearing variant lowers to a frozen dataclass; pydantic validates dataclasses natively, so
a variant used as a field type round-trips through construction and `model_dump`.

```by
from pydantic import BaseModel

enum class Shape:
    case Circle(radius: float)
    case Square(side: float)

class Drawing(BaseModel):
    name: str
    shape: Shape.Circle

d = Drawing(name="fig", shape=Shape.Circle(2.0))
assert d.shape.radius == 2.0
assert d.model_dump() == {"name": "fig", "shape": {"radius": 2.0}}
```

## an explicit variant union as a field type

the variants of a payload enum can be joined explicitly; pydantic validates the union of the two
frozen dataclasses.

```by
from pydantic import BaseModel

enum class Shape:
    case Circle(radius: float)
    case Square(side: float)

class Drawing(BaseModel):
    shape: Shape.Circle | Shape.Square

d = Drawing(shape=Shape.Square(3.0))
assert d.model_dump() == {"shape": {"side": 3.0}}
```

## optional chaining on optional fields

an optional field (`T | None`) is exactly the operand `?.` expects: the chain short-circuits to
`None` when the field is absent, and the checker unions the `None` in once at the end of the chain.

```by
from pydantic import BaseModel

class Node(BaseModel):
    label: str | None = None

reveal_type(Node(label="hi").label?.upper())  # revealed: str | None
assert Node(label="hi").label?.upper() == "HI"
assert Node().label?.upper() is None
```

## checked cast of a value pulled from `model_dump`

`model_dump()` returns a loosely-typed mapping; `cast!` narrows a value out of it and verifies the
claim at runtime.

```by
from pydantic import BaseModel

class Item(BaseModel):
    name: str
    count: int

i = Item(name="x", count=3)
name = i.model_dump()["name"] cast! str
reveal_type(name)  # revealed: str
assert name == "x"
```

## a generic model round-trips through validation

a pydantic generic model implements `__class_getitem__`, so `Model[int](...)` is native pydantic.
the reified-generics pass only wraps *functions*, never a class, so the model is never given a
`@generic` wrapper — the specialized constructor reaches pydantic untouched.

```by
from pydantic import BaseModel

class Box[T](BaseModel):
    value: T

b = Box[int](value=5)
reveal_type(b.value)  # revealed: int
assert b.value == 5
assert b.model_dump() == {"value": 5}
```

## class-body mutable defaults are not re-evaluated

the mutable-defaults transform rewrites *function* default arguments; a class-body field default is
untouched. pydantic deep-copies field defaults per instance itself, so two instances get independent
lists.

```by
from pydantic import BaseModel

class Bag(BaseModel):
    items: list[int] = []

a = Bag()
a.items.append(1)
b = Bag()
assert a.items == [1]
assert b.items == []
```
