# identity and isinstance

basedpython swaps the surface syntax for identity comparison and `isinstance`
checks: `===` is identity and `is` is an instance check

```by
if x === y:
    ...
if x !== y:
    ...
if x is int:
    ...
if x is not str:
    ...
```

transpiles to:

```python
if x is y:
    ...
if x is not y:
    ...
if isinstance(x, int):
    ...
if not isinstance(x, str):
    ...
```

| basedpython  | Python output          |
| ------------ | ---------------------- |
| `x === y`    | `x is y`               |
| `x !== y`    | `x is not y`           |
| `x is y`     | `isinstance(x, y)`     |
| `x is not y` | `not isinstance(x, y)` |

## why

`isinstance(x, T)` is the dominant runtime check; `is` for object identity is
rare outside of `is None`. basedpython promotes the common case to a keyword
and demotes identity to a triple-equals operator borrowed from JavaScript

## checking against `None`

`a is None` and `a is not None` stay as python identity checks. `a === None`
spells the same thing

## checking against values

`isinstance` requires a class as its second argument, so a rhs that resolves
to a plain *value* keeps python identity semantics. this covers literal
singletons (`None`, `True`/`False`, numbers, strings, `...`), enum members,
and any other rhs whose static type is an instance rather than a class:

```py
enum class Genre:
    case A, B

Genre.A is not Genre.B  # stays `is not` — members are singleton instances
```

a payload-bearing variant (`Shape.Circle`) *is* a class, so `x is Shape.Circle` lowers to `isinstance(x, Shape.Circle)` as usual

## interaction with `==`

`==` is unchanged — it still calls `__eq__` exactly as in Python. only `is`
and `===` are remapped

## scope

the swap applies to every comparison in source, with the value-rhs exemption
above decided from static types. there is no opt-out at the statement level —
write `===` / `!==` whenever you mean identity. ty understands both forms
when type-checking `.by` files
