# typed dict literals

a dict-shaped expression in an annotation position is a `TypedDict`:

```by
def get_user() -> {"name": str, "age": int}: ...
```

shape identity ignores key order — `{"a": int, "b": str}` and
`{"b": str, "a": int}` are the same type

`typing_extensions` is a runtime requirement of any module that contains a
typed-dict-literal annotation (the generated type uses PEP 728
`closed`/`extra_items` features that aren't yet in `typing.TypedDict`)

## closed by default, `**: T` for extra items

dict literal types are closed by default, so extra keys are rejected. a
`**: T` entry switches that to "extra keys allowed, must match T":

```by
a: {"key": int}              # closed — extra keys rejected
b: {"key": int, **: str}     # extra keys allowed, must be str
```

## nested shapes

nested typed-dict literals work transparently:

```by
addr: {"city": str, "zip": str}
user: {"name": str, "address": {"city": str, "zip": str}}
```

## type variables

a dict literal type is not a generic class of its own, but its fields can name the type variables
of the scope it is written in. specializing that scope substitutes them:

```by
class B[T]:
    def get(self) -> {"a": T}: ...

def f(b: B[int]):
    reveal_type(b.get())  # {"a": int}
```

a [keyword-variadic pack](keyword-variadic.md) splices its whole field list in with `{**Kwargs}`

## display

a dict literal type reads back as its shape rather than as the generated class name, with keys
quoted the way the source spells them and fields ordered by key:

```by
def f(x: {"name": str, "age": int}):
    reveal_type(x)  # {"age": int, "name": str}
```

a pack that hasn't been specialized yet shows as the pending splice — `{"tag": int, **Kwargs@A}`

## lowering

each shape is hoisted to one module-level `TypedDict` class. those classes land ahead of
everything the module defines, and a field can name a class declared later or a type parameter
that only exists inside the generic class the literal was written in, so field types are emitted
as forward references:

```python
class _TypedDict_<hash>(TypedDict, closed=True):
    name: "str"
    age: "int"
```

a `{**Kwargs}` splice has no fields to erase to — they are only known at the specialization site,
and python erases type arguments anyway — so it lowers to `extra_items="object"`

## scope

the rewrite fires only in annotation positions — function parameter and
return annotations, variable annotations, and nested type arguments. dict
expressions in value positions (real dicts) are never affected
