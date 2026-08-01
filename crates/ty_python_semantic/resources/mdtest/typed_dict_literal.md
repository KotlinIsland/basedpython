# basedpython: typed dict literal type expressions

In basedpython, `{"key": T, ...}` in a type position is sugar for an inline `typing.TypedDict`
subclass. ty synthesizes a `TypedDict` class per unique shape (matching the transpiler) so key
access on an instance resolves to the field's declared type. Identity is shape-based: two
structurally identical typed-dict literals in the same file resolve to the same class.

## Type-expression position

### As a variable annotation

```by
def make(name: str, age: int) -> None:
    a: {"name": str, "age": int} = {"name": name, "age": age}
    reveal_type(a["name"])  # revealed: str
    reveal_type(a["age"])  # revealed: int
```

### As a parameter annotation

```by
def f(x: {"name": str, "age": int}) -> None:
    reveal_type(x["name"])  # revealed: str
    reveal_type(x["age"])  # revealed: int
```

### As a return annotation

```by
def g() -> {"name": str, "age": int}:
    return {"name": "asdf", "age": 1}

reveal_type(g()["name"])  # revealed: str
reveal_type(g()["age"])  # revealed: int
```

### Single-field typed dict

```by
def h(x: {"only": int}) -> None:
    reveal_type(x["only"])  # revealed: int
```

### Extra-items marker `**: T`

A `**: T` entry in a dict literal type lowers to `extra_items=T` on the synthesized `TypedDict`.
ty's extra-items semantics are still TODO, so the declared fields still type-check but extra keys
aren't yet enforced.

```by
def f(x: {"name": str, **: int}) -> None:
    reveal_type(x["name"])  # revealed: str
```

## Display

A synthesized `TypedDict` reads back as the shape it was written as — its generated class name is a
hash and says nothing. Keys are quoted the way the source spells them.

```by
def f(x: {"name": str, "age": int}) -> None:
    reveal_type(x)  # revealed: {"age": int, "name": str}
```

Fields are ordered by key, so two spellings of the same shape display identically.

```by
def f(x: {"b": str, "a": int}) -> None:
    reveal_type(x)  # revealed: {"a": int, "b": str}
```

The generated class name must not surface through an alias either — a typed dict's inhabitants are
`dict`s at runtime whichever way the annotation names the shape.

```by
type Point = {"x": int}


def f(p: Point, q: {"x": int}) -> None:
    reveal_type(type(p))  # revealed: <class 'dict[str, object]'>
    reveal_type(type(q))  # revealed: <class 'dict[str, object]'>
```

## Type variables in a dict literal type

A dict-literal type is not a generic class of its own, but its fields can mention the type variables
of the scope it is written in. Specializing that scope substitutes them.

### Specializing the enclosing class

```by
class B[T]:
    def get(self) -> {"a": T}:
        raise NotImplementedError

def f(b: B[int]):
    reveal_type(b.get())  # revealed: {"a": int}
    reveal_type(b.get()["a"])  # revealed: int
```

### Unspecialized, the type variable stays

```by
class B[T]:
    def get(self) -> {"a": T}:
        reveal_type(self.get())  # revealed: {"a": T@B}
        raise NotImplementedError
```

### A generic function

```by
def g[T](x: T) -> {"value": T}:
    raise NotImplementedError

def f(s: str, i: int):
    reveal_type(g(s))  # revealed: {"value": str}
    reveal_type(g(i)["value"])  # revealed: int
```

### Nested in another type

```by
class B[T]:
    def get(self) -> list[{"a": T}]:
        raise NotImplementedError

def f(b: B[str]):
    reveal_type(b.get())  # revealed: list[{"a": str}]
```

### Variance

A type variable that only ever appears inside a dict literal type still has a variance. Its fields
are mutable, so it is invariant in them.

```by
class B[T]:
    def get(self) -> {"a": T}:
        raise NotImplementedError

def f(b: B[int]):
    x: B[str] = b  # error: [invalid-assignment]
```

## Python-passthrough — dict literal in type position is still an error

```py
a: {"name": str, "age": int}  # error: [invalid-type-form]
```
