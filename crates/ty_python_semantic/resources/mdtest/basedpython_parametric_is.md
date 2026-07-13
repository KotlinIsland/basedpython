# basedpython: parametric type tests

`x is C[args]` (keyword form) tests a value against a *specialization*. The test is resolved
rust-style from static types wherever possible. When it can't be — the value's type is dynamic or
erased — the last resort is a runtime probe of the value's `__orig_class__`. That works for a
user-defined generic (whose instances carry it) but never for a builtin collection (whose instances
erase their type arguments), so a probe against a builtin is an error.

## statically decided tests are silent

```by
xs = [1, 2]
ys: list[object] = [1]

b1 = xs is list[int]
b2 = ys is list[int]
```

## reified type parameters carry the answer

The test on `x: T` lowers to an equality check of the reified `T` cell, so it is verified — and it
reifies `T` (the function transpiles with the `@generic` wrapper).

```by
def f[T](x: T) -> bool:
    return x is list[int]

def g[T](x: list[T]) -> bool:
    return x is list[int]
```

## unions of disjoint specializations are verified by witness

```by
def f(x: list[int] | list[str]) -> bool:
    return x is list[int]
```

## a builtin target on a dynamic value is an error

A builtin collection built at runtime carries no record of its type arguments, so the probe can
never succeed.

```by
def f(x) -> bool:
    return x is list[int]  # error: [erased-type-check]
```

## an erased type parameter against a builtin is an error

The value's type reaches the test through a local, so no parameter annotation ties it to `T`; the
value is a runtime `list`, which is erased.

```by
def f[T](x: T) -> bool:
    y = [x]
    return y is list[int]  # error: [erased-type-check]
```

## a wide static type against a builtin is an error

```by
def f(x: object) -> bool:
    return x is list[int]  # error: [erased-type-check]
```

## a user-defined generic target is valid

`A`'s instances carry `__orig_class__` (stamped by `A[int](…)`), so the runtime probe is a
legitimate check — no diagnostic.

```by
class A[T]:
    def __init__(self, t: T): ...

def f(x) -> bool:
    return x is A[int]

def g(x: object) -> bool:
    return x is A[int]
```

## positive narrowing

The positive branch narrows to the tested specialization. The negative branch does not narrow: an
unreified or empty value answers `False` even when its static type matches.

```by
def f(x: list[int] | list[str]):
    if x is list[int]:
        reveal_type(x)  # revealed: list[int]
    else:
        reveal_type(x)  # revealed: list[int] | list[str]
```

## `===` keeps identity semantics

```by
xs = [1]

b = xs === list[int]
```
