# `TypedDict` and `Self` in type parameters

`TypedDict` and `Self` are accepted where a type parameter declares its bound or its default:

```python
def f[T: TypedDict](x: T) -> T: ...

class Node:
    def link[T: Self](self, other: T) -> T: ...

class Ctx[T = Self]:
    def __enter__(self) -> T: ...
```

these are type-checking enhancements with no new syntax, so they apply to `.py` files as well
as `.by` files

## motivation

both spellings are natural to reach for and neither was expressible:

- `T: TypedDict` — "any typed dictionary", requested in
    [typing#1395](https://github.com/python/typing/issues/1395) and
    [mypy#11030](https://github.com/python/mypy/issues/11030). without it, a function that accepts
    an arbitrary typed dictionary has to fall back to `Mapping[str, object]`, which discards every
    key and value type
- `T = Self` — "generic in the receiver's own type", discussed in
    [self as a typevar default](https://discuss.python.org/t/self-as-typevar-default/90939). the
    motivating case is `contextlib.AbstractContextManager`, whose `__enter__` returns the object it
    was called on, but whose stub has to name a type parameter instead

## `TypedDict` as an upper bound

bare `TypedDict` is not a type expression anywhere else — as a bound it denotes the top of the
`TypedDict` lattice, so the type parameter ranges over every typed dictionary and nothing else:

```python
class Movie(TypedDict):
    name: str

class Book(TypedDict):
    title: str
    pages: int

def f[T: TypedDict](x: T) -> T:
    return x

reveal_type(f(Movie(name="a")))            # Movie
reveal_type(f(Book(title="b", pages=1)))   # Book

f(1)                # error: `Literal[1]` does not satisfy upper bound `TypedDict`
f({"name": "a"})    # error: `dict[str, str]` does not satisfy upper bound `TypedDict`
```

the argument keeps its own precise type, which is the point — `Mapping[str, object]` would have
erased it

this works for generic classes, `type[T]`, and legacy `TypeVar`s:

```python
class Wrapper[T: TypedDict]:
    def __init__(self, value: T) -> None:
        self.value = value

reveal_type(Wrapper(Movie(name="a")).value)   # Movie

def from_class[T: TypedDict](cls: type[T]) -> T: ...

reveal_type(from_class(Movie))                # Movie

L = TypeVar("L", bound=TypedDict)
```

### unpacking a `TypedDict`-bounded type parameter

`**kwargs: Unpack[T]` normally expands into keyword-only parameters as soon as the signature is
built. when `T` is a type parameter it cannot expand yet, so the parameter stays put and expands
once `T` is solved — the same deferral used by [keyword-variadic packs](keyword-variadic.md):

```python
class Thing(TypedDict):
    name: str
    count: int

class A[ExtraArgs: TypedDict]:
    def do_it(self, **extra: Unpack[ExtraArgs]) -> None: ...

a: A[Thing] = A()
a.do_it(name="x", count=1)
a.do_it(name=1, count=1)   # error: expected `str`, found `Literal[1]`
a.do_it(name="x")          # error: no argument provided for `count`
```

this is the example from typing#1395. note that the typing spec requires a concrete `TypedDict`
here — accepting a type parameter is a deliberate extension

when nothing else pins it down, `T` is solved from the keyword arguments as a whole:

```python
def g[T: TypedDict](**kw: Unpack[T]) -> T: ...

reveal_type(g(name="a"))   # <TypedDict with items 'name'>
```

that only holds while the keyword arguments are the *sole* source of `T`. if another parameter
also mentions it, that parameter decides `T` before these keywords are matched, and the deferred
parameter could never be re-checked against it — so the signature is rejected rather than
silently going unchecked:

```python
# error: unpacked value for `**kwargs` must be a TypedDict, not `T@f`
def f[T: TypedDict](proto: T, **kw: Unpack[T]) -> T: ...
```

## `Self` as an upper bound

a method's type parameter can be bounded by `Self`, restricting it to the receiver's own type:

```python
class Node:
    def link[T: Self](self, other: T) -> T:
        return other

class Unrelated: ...

def _(node: Node) -> None:
    node.link(Unrelated())   # error: `Unrelated` does not satisfy upper bound `Self@link`
```

`Self` is bound by the enclosing class rather than by the type parameter list, so it is exempt
from the rule that a bound cannot be generic. every other type parameter is still rejected there:

```python
def g[U, T: U](x: T) -> T: ...   # error: TypeVar upper bound cannot be generic
```

## `Self` as a default

a class type parameter can default to `Self`, so a class used bare is generic in the receiver's
own type. `Self` stays symbolic until a member is looked up, so it stays exact however deep the
subclassing goes:

```python
class Ctx[T = Self]:
    def __enter__(self) -> T: ...
    def __exit__(self, *args: object) -> None: ...

class Sub(Ctx): ...
class SubSub(Sub): ...

with Sub() as x:
    reveal_type(x)      # Sub

with SubSub() as y:
    reveal_type(y)      # SubSub
```

an explicit type argument still wins over the default:

```python
class Explicit(Ctx[int]): ...

def _(e: Explicit) -> None:
    reveal_type(e.__enter__())   # int
```

## rejected forms

`TypedDict` is only a type expression as a bound. elsewhere it is still an error, including as a
constraint, since constraints enumerate concrete types and the top of the lattice is not one:

```python
def f(x: TypedDict) -> None: ...      # error: not allowed in parameter annotations

T = TypeVar("T", TypedDict, int)      # error: not allowed in type expressions
```

`Self` can be a class type parameter's *default* but not its *bound*:

```python
class C[T: Self]: ...   # error: `Self` cannot bound a type parameter of the class it belongs to
```

as a default, `Self` stays symbolic and is bound when a member is looked up on a receiver. as a
bound there is no such moment — specializing the class (`C[X]`) happens where no receiver exists,
so the bound could never be checked

`Self` outside a class remains an error, as it is in python:

```python
def f[T: Self](x: T) -> T: ...   # error: `Self` is not allowed in a type expression
```
