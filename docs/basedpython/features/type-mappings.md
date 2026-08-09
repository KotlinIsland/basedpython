# type mappings

a type parameter written with `in` ranges over a *type mapping* — the set of
types it is allowed to be — instead of sitting beneath an upper bound:

```by
def f[T in (int, str)](x: T) -> T: ...
```

transpiles to:

```python
def f[T: (int, str)](x: T) -> T: ...
```

python has only the one spelling: after a `:`, a tuple means constraints. that
leaves no way to write a tuple *type* as a bound, so basedpython gives the
constraint set its own keyword and lets `:` keep meaning what it says

## syntax

```by
def f[T in (int, str)](x: T) -> T: ...

class Container[T in (int, str, bytes)]: ...

type Alias[T in (int, str)] = list[T]
```

`in` follows the parameter's name, so it composes with the modifiers written
ahead of it — [variance](variance.md), [`reified`](reified-generics.md):

```by
class Container[in out T in (int, str)]: ...
```

## semantics

`T in (int, str)` declares a constrained typevar — `T` can only be specialized
to exactly `int` or `str`, never a subtype and never a union:

```by
def f[T in (int, str)](x: T) -> T:
    return x

reveal_type(f(1))    # int
reveal_type(f("a"))  # str
```

`T: (int, str)` is something else entirely: a typevar whose upper bound is the
type `tuple[int, str]`

```by
# a mapping: T is int or str
def constrained[T in (int, str)](x: T) -> T: ...

# a bound: T must be a subtype of tuple[int, str]
def bounded[T: (int, str)](x: T) -> T: ...
```

### at least two members

a mapping of one leaves nothing to choose between, and python rejects it too:

```by
# error: [invalid-type-variable-constraints] TypeVar must have at least two constrained types
def f[T in (int,)](): ...
```

### `in` is not a range

a mapping is an unordered set, so it has no top to hang a
[bound range](bound-ranges.md) from. the two forms are alternatives, not
combinable

## polyfill

on targets below python 3.12 the mapping becomes a legacy `TypeVar` with
positional constraint arguments:

```python
# generated for python < 3.12
from typing import TypeVar
_T = TypeVar("_T", int, str)
def f(x: _T) -> _T: ...
```

on 3.12+ the mapping keeps pep 695 syntax and becomes python's constraint
tuple:

```python
# generated for python 3.12+
def f[T: (int, str)](x: T) -> T: ...
```

that rewrite is the [pep 695 polyfill](polyfills.md) at work, not this feature

## see also

- [type parameter bound ranges](bound-ranges.md) — `T: Lower..Upper`, an
    ordered interval rather than a set
- [bounds on a variadic pack](pack-bounds.md) — `*Ts: *(int, str)`
- [generics](generics.md)
