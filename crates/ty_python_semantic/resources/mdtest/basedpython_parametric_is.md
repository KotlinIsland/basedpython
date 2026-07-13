# basedpython: parametric type tests

`x is C[args]` (keyword form) tests a value against a *specialization*. The test is resolved
rust-style from static types wherever possible; when it can be verified neither statically nor at
runtime (through a reified type parameter or a witness element), the checker warns and the lowering
falls back to probing the value's `__orig_class__`, answering `False` for values that carry none.

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

## dynamic values are unchecked

```by
def f(x) -> bool:
    return x is list[int]  # error: [unchecked-type-check]
```

## erased type parameters are unsafe

The value's type reaches the test through a local, so no parameter annotation ties it to `T` and `T`
stays erased.

```by
def f[T](x: T) -> bool:
    y = [x]
    return y is list[int]  # error: [unchecked-type-check]
```

## a wide static type is unchecked

`object` does not exclude `list[int]`, but cannot verify it either.

```by
def f(x: object) -> bool:
    return x is list[int]  # error: [unchecked-type-check]
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
