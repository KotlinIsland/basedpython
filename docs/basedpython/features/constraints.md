# explicit typevar constraints

basedpython requires an explicit `constraints` keyword to declare a constrained
typevar, making the intent unambiguous:

```by
def f[T: constraints (int, str)](x: T) -> T: ...
```

transpiles to:

```python
def f[T: (int, str)](x: T) -> T: ...
```

the keyword is erased — `constraints (int, str)` is exactly python's
`(int, str)` constraint tuple. targeting below 3.12 additionally rewrites the
type parameter into a `TypeVar`, but that is the [pep 695 polyfill](polyfills.md)
at work, not this feature

## motivation

in Python, typevar constraints use a tuple literal as the bound annotation:

```python
def f[T: (int, str)](x: T) -> T: ...
```

this is ambiguous: `(int, str)` looks like a tuple type being used as an upper bound, not
a list of discrete constraints. the distinction only becomes clear from context

## syntax

```by
def f[T: constraints (int, str)](x: T) -> T: ...

class Container[T: constraints (int, str, bytes)]: ...

type Alias[T: constraints (int, str)] = list[T]
```

## semantics

`T: constraints (int, str)` declares a constrained typevar — `T` can only be specialized to
exactly `int` or `str`, never a subtype or union

`T: (int, str)` is a typevar with an upper bound of type `tuple[int, str]`, not constraints

```by
# constraints: T is int or str
def constrained[T: constraints (int, str)](x: T) -> T: ...

# bound: T must be a subtype of tuple[int, str]
def bounded[T: (int, str)](x: T) -> T: ...
```

### minimum two types required

a constrained typevar must have at least two constraint types:

```by
# error: TypeVar must have at least two constrained types
def f[T: constraints (int,)](): ...
```

## polyfill

on targets below Python 3.12, `constraints (int, str)` is polyfilled to a legacy `TypeVar`
with positional constraint arguments:

```python
# generated for Python < 3.12
from typing import TypeVar
_T = TypeVar("_T", int, str)
def f(x: _T) -> _T: ...
```

on Python 3.12+, the `constraints` keyword is stripped and the standard tuple constraint
syntax is emitted:

```python
# generated for Python 3.12+
def f[T: (int, str)](x: T) -> T: ...
```
