# dynamic

basedpython spells the dynamic type `dynamic` instead of importing `Any`:

```by
x: dynamic
def f(x: dynamic) -> dynamic: ...
xs: list[dynamic]
```

transpiles to:

```python
from typing import Any

x: Any
def f(x: Any) -> Any: ...
xs: list[Any]
```

`dynamic` is the surface spelling of `typing.Any`; the rewrite pulls in
`from typing import Any` (skipped when `Any` is already imported)

## composition

`dynamic` is a plain type name, so it composes with every type constructor —
unions, generics, `Callable`, `Annotated`, `cast`, type aliases, type-param
bounds:

| basedpython                    | Python output          |
| ------------------------------ | ---------------------- |
| `dynamic`                      | `Any`                  |
| `dynamic \| None`              | `Any \| None`          |
| `dict[str, dynamic]`           | `dict[str, Any]`       |
| `Callable[[dynamic], dynamic]` | `Callable[[Any], Any]` |
| `Annotated[dynamic, meta]`     | `Annotated[Any, meta]` |

## scope

the rewrite fires only in type-expression positions recognised by the shared
type-position walker — annotations, return types, type-alias right-hand sides,
type-param bounds and defaults, class bases, value-position type applications
(`reveal_type(list[dynamic])`), the first argument of `cast` and `Annotated`,
and `Callable` parameter lists

outside a type position, `dynamic` is an ordinary identifier — `dynamic = 5`
and `print(dynamic)` pass through untouched. a local binding shadows the
keyword, so `dynamic = int; x: dynamic` keeps `x: dynamic`

## reverse

the round-trip rewrites `Any` back to `dynamic` in annotation positions, so
standard-Python `typing.Any` reads as idiomatic basedpython. only a name that
resolves to the `typing.Any` special form is rewritten — a shadowed
`Any = object()` or an unrelated name is left alone
