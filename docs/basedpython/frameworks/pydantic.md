# pydantic support

basedpython has full support for pydantic v2 models. type checking is precise, and basedpython features work correctly inside model bodies and methods.

## what works

### model classes and fields

- pydantic models defined in `.by` code check precisely
- field types are inferred from annotations and field specifiers
- `Field(...)` specifiers with defaults, constraints, and configuration
- generic models: `class Box[T](BaseModel): value: T` with reified constructor calls `Box[int](value=1)`
- computed fields via `@computed_field` properties
- model validators via `@field_validator` and `@model_validator`

### transpilation compatibility

- `optional?.chaining` works correctly on optional fields
- `checked cast` and `cast?` operators work inside model methods
- reified generics: `Model[int](...)` constructor calls work natively
- mutable default re-evaluation is handled correctly

### runtime schema generation

- `.by` models generate valid pydantic schemas
- float and complex fields work correctly at runtime (previously crashed schema generation)

## limitations and workarounds

### bare enum-union names as field types

if you use a basedpython enum as a field type, you'll get validation errors at runtime if you use the bare union name:

```by
class Shape:
    case Circle
    case Square

class Drawing(BaseModel):
    shape: Shape  # error at runtime: pydantic can't validate
```

**workaround:** use an explicit variant union or a single variant instead:

```by
shape: Shape.Circle | Shape.Square  # works
shape: Shape.Circle  # also works
```

payload enums (with associated values) also require explicit unions.

### `model_dump()` return type

`model_dump()` returns `Unknown` rather than `dict[str, Any]`. this is sound but less precise than ideal. use a type assertion if you need to treat the result as a dict:

```by
data = model.model_dump() as dict[str, object]
```

### enum member identity checks

`x is EnumMember` doesn't work correctly — identity checks on enum values lower to `isinstance`, which crashes at runtime. use `==` instead:

```by
if status == Status.Active:  # correct
    ...

if status is Status.Active:  # don't do this
    ...
```

## required setup

pydantic is detected automatically when installed. if you're using pydantic v2, you already have inline type stubs (`py.typed`), so there's nothing else to set up.

## incompatible patterns

**`init` shorthand in a model body**

pydantic synthesizes `__init__` with its own validation and instrumentation. declaring an `init` method silently changes these semantics and breaks validation. you'll get an error:

```by
class User(BaseModel):
    name: str
    
    init(name: str):  # error: conflicts with pydantic's __init__
        self.name = name
```

**use:** let pydantic synthesize the constructor. if you need custom initialization logic, use a validator instead.

**`data class` modifier**

stacking `@dataclass` on a pydantic model metaclass is runtime-broken. the transpiler prevents this with an error.

**use:** pydantic models are already `dataclass`-like; you don't need the decorator.

## examples

basic model:

```by
from pydantic import BaseModel, Field

class User(BaseModel):
    id: int
    name: str
    email: str | None = None
    age: int = Field(gt=0, default=18)

user = User(id=1, name="Alice")  # type checks and validates
```

generic model:

```by
from pydantic import BaseModel

class Container[T](BaseModel):
    value: T
    label: str

int_box = Container[int](value=42, label="answer")  # reification works
str_box = Container[str](value="hello", label="greeting")
```

computed field:

```by
from pydantic import BaseModel, computed_field

class User(BaseModel):
    first: str
    last: str
    
    @computed_field
    def full_name(self) -> str:
        return f"{self.first} {self.last}"
```

## see also

- [pydantic documentation](https://docs.pydantic.dev/latest/)
- framework compatibility matrix in the [frameworks overview](index.md#basedpython-features-and-framework-compatibility)
