# basedpython: a decorator on a binding

Python allows a decorator only on a `def` or a `class`. basedpython allows one above a binding too,
where it attaches metadata to the binding's type — the same thing writing the decorator in the
[type position](basedpython_decorated_type.md) does, put where a long decorator reads better.

```toml
[environment]
python-version = "3.12"
```

## the binding keeps its own type

The decorator annotates; it does not wrap. `Annotated[T, …]` is `T`, so the name holds what was
written under it.

```by
def tag(label: str) -> str:
    return label

@tag("units")
x: int = 1

reveal_type(x)  # revealed: 1
```

## the value is checked against the declared type

```by
def tag(label: str) -> str:
    return label

@tag("units")
x: int = "not an int"  # error: [invalid-assignment]
```

## a chain is read innermost first

The one written closest to the binding is the innermost, the order `@a @b int` puts them in.

```by
def tag(label: str) -> str:
    return label

@tag("outer")
@tag("inner")
x: int = 1

reveal_type(x)  # revealed: 1
```

## a declaration keeps its `Final`

```by
def tag(label: str) -> str:
    return label

@tag("units")
let a: int = 1

reveal_type(a)  # revealed: 1
```

## on a class attribute

```by
def tag(label: str) -> str:
    return label

class A:
    @tag("units")
    let a: int = 1

reveal_type(A.a)  # revealed: int
```

## an unresolved decorator is reported

The decorator is an ordinary expression, evaluated where it is written.

```by
@nope  # error: [unresolved-reference]
x: int = 1
```

## a binding with no type has nothing to annotate

Inferring one would make what the decorator lands on depend on what the value happened to be.

```by
# error: [invalid-syntax] "a decorator on a binding annotates its type, so the binding needs one"
@nope
a = 1
```

## a declaration with no value is not a binding

```by
def tag(label: str) -> str:
    return label

# error: [invalid-syntax] "a declaration with no value binds nothing for a decorator to annotate"
@tag("units")
a: int
```

## anything that is not a definition or a binding is rejected

```by
def tag(label: str) -> str:
    return label

# error: [invalid-syntax] "Expected a definition or a binding after decorator"
@tag("units")
pass
```
