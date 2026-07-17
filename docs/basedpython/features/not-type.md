# negation types

`not T` in an annotation position is a type that excludes `T`:

```by
def f(x: not int) -> None: ...

f("a")  # ok
f()1)   # error
```

transpiles to:

```python
from ty_extensions import Not

def f(x: Not[int]) -> None: ...
```

`Not[T]` is the basedpython type-system primitive for negation. it is sourced
from `ty_extensions` and ty narrows accordingly: a value of type `Not[int]`
will fail to type-check against `int`-typed positions

## composition

`not` composes with `|` (union) and `&` (intersection) as well as nested
generics:

| basedpython        | Python output     |
| ------------------ | ----------------- |
| `not int`          | `Not[int]`        |
| `not (int \| str)` | `Not[int \| str]` |
| `list[not int]`    | `list[Not[int]]`  |
| `not int \| str`   | `Not[int] \| str` |

precedence follows the source: parenthesize when the negation should bind
across a union or intersection

## scope

the rewrite fires only in syntactic annotation positions:

- function parameter annotations
- function return annotations
- variable annotations (`x: not int`)
- nested type arguments inside an annotation

`not x` in a value context — boolean negation — is never affected. the
transform is structural, not type-aware
