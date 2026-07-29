# typevar variance keywords

basedpython adds `in` and `out` keywords on PEP 695 type parameters to
declare variance directly at the declaration site:

```by
class Source[out T]: ...
class Sink[in T]: ...
class Both[in out T]: ...
```

`out T` declares `T` covariant, `in T` contravariant, and `in out T`
invariant. a *bare* `T` declares nothing: its variance is **inferred** from
what the class body does with it, so it can come out covariant, contravariant,
invariant or bivariant. writing a keyword is how you pin one down and stop the
inference from following the body:

```by
class Bare[T]:
    def get(self) -> T: ...        # inferred covariant

class Pinned[in out T]:
    def get(self) -> T: ...        # invariant, because it says so
```

`Bare[int]` is assignable to `Bare[object]`; `Pinned[int]` is not. variance
affects subtyping in the obvious way:

- `Source[Dog]` is assignable to `Source[Animal]` (covariant — `T` is
    produced)
- `Sink[Animal]` is assignable to `Sink[Dog]` (contravariant — `T` is
    consumed)
- `Both[Dog]` and `Both[Animal]` are assignable to neither (invariant)

## transpilation

on Python 3.12+ the keywords are stripped because PEP 695 itself does not
yet support inline variance declarations:

```python
class Source[T]: ...
class Sink[T]: ...
class Both[T]: ...
```

on pre-3.12 targets the keywords are passed through to the `TypeVar` polyfill,
which emits the corresponding `covariant=True` / `contravariant=True`
arguments:

```python
_T = TypeVar("_T", covariant=True)
class Source(Generic[_T]): ...

_T_contra = TypeVar("_T_contra", contravariant=True)
class Sink(Generic[_T_contra]): ...
```

## scope

variance keywords are recognized in two surface positions:

1. on a PEP 695 type-parameter declaration (`class C[out T]:`), as
    shown above — affects the **declared** variance of `T`
1. on a subscript argument (`list[out int]`), described below — affects
    only **this one annotation** without touching `list`'s declaration

they are not allowed on bare `TypeVar(...)` calls (use the `covariant=` /
`contravariant=` arguments directly there).

## use-site variance

writing `Container[out T]`, `Container[in T]`, or `Container[in out T]`
gives an annotation a read-only, write-only, or read-write view over a
generic container, without affecting the container's own declared
variance:

```by
def read(data: list[out int]):
    data[0]        # int
    data[0] = 1    # error — write rejected

def write(data: list[in int]):
    data[0] = 1    # ok — int accepted
    data[0]        # error — read rejected

def both(data: list[in out int]):
    data[0]        # int
    data[0] = 1    # ok
```

the container keeps its nominal identity — the projection rides along as a
per-parameter tag, so `list[out int]` and `set[out int]` stay unrelated
types. each argument of a multi-argument subscript is tagged independently,
and an unmarked argument is simply untagged:

```by
def f(m: dict[str, out int]):
    m["a"]         # int
    m["a"] = 1     # error — write rejected through the `out` view
```

a projection only ever *adds* to what the declaration allows. against a
declared-invariant parameter, `out` relaxes the position to covariant and
`in` to contravariant; against a parameter already declared `out T` or
`in T`, the declared variance covers everything the projection could give
and the projection is a no-op.

the projection is part of the type, so it also decides a
[parametric type test](parametric-type-tests.md) — `a is A[out int]` matches
covariantly even when `A`'s `T` is invariant.

## inlay hints

a type parameter that declares no variance gets its inferred one as an inlay
hint, written where the keyword would go:

```by
class Source[⟨out ⟩T]:
    def get(self) -> T: ...

class Sink[⟨in ⟩T]:
    def put(self, value: T) -> None: ...
```

a generic type alias is hinted the same way:

```by
type Alias[⟨in out ⟩T] = list[T]
```

a parameter ty infers as bivariant is not hinted — basedpython has no spelling
for it
