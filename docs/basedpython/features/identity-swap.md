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

## a test that can never hold

`non-overlapping-type-test` warns when the value's type and the tested type share
no value at all. the test is then a constant — `is` never holds, `is not` always
does — so either the branch it guards is dead or the wrong type was named:

```by
class Shape: ...

def f(x: None, s: Shape):
    if x is int: ...      # warning: `None` and `int` are non-overlapping
    if s is str: ...      # warning: `Shape` and `str` are non-overlapping

def g(o: object):
    if o is int: ...      # ok — `object` overlaps `int`
```

the value's *narrowed* type is what is tested, so it is often sharper than the
declaration: `c = 1` is a `Literal[1]`, and a constructor call is
[`final A`](type-modifiers.md#a-constructor-call-is-inferred-final) — a value
whose runtime class is exactly `A`'s, and therefore not a `str` and not a
subclass of `A` either.

a [parametric target](parametric-type-tests.md) is judged by the same fold that
decides the test, so a use-site variance projection (`a is A[out int]`) that
makes the test possible keeps it quiet. a union target is never reported: any arm
matching makes the whole test hold

## interaction with `==`

`==` is unchanged — it still calls `__eq__` exactly as in Python. only `is`
and `===` are remapped

## scope

the swap applies to every comparison in source, with the value-rhs exemption
above decided from static types. there is no opt-out at the statement level —
write `===` / `!==` whenever you mean identity. ty understands both forms
when type-checking `.by` files
