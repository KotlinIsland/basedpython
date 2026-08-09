# differences from python

`.by` is not a superset of `.py`. almost all python means the same thing in
basedpython, and this page is the rest of it: every construct that reads
differently, so that renaming a `.py` file to `.by` is a decision rather than a
formality

each one is a deliberate fix to something python cannot change without breaking
the world

!!! tip "you don't have to port by hand"

    [`by transpile --reverse`](../getting-started.md#converting-python-to-basedpython)
    rewrites python source into basedpython idioms, including the constructs
    below

## runtime behaviour

the same source, running, does something else

### `is` is an instance check

`is` means `isinstance` and `===` means identity:

| you write    | python does            |
| ------------ | ---------------------- |
| `x is y`     | `isinstance(x, y)`     |
| `x is not y` | `not isinstance(x, y)` |
| `x === y`    | `x is y`               |
| `x !== y`    | `x is not y`           |

the compiler doesn't always do `isinstance`, for example `x is None` will become `x is None` in
python, this is because "type of x is None" and "value of x is None" have identical meanings

see [identity and isinstance](identity-swap.md)

### a mutable default is re-evaluated per call

```by
def append_one(items=[]):
    items.append(1)
    return items
```

python returns an ever-growing list; basedpython returns `[1]` every time. only
non-scalar defaults are affected — numbers, bools, strings, `None` and `...`
stay as plain python defaults. see
[default argument re-evaluation](mutable-defaults.md)

### a loop target is a fresh binding per iteration

```by
fns = []
for i in [1, 2, 3]:
    fns.append(lambda: print(i))
```

python prints `3 3 3`, because the loop has one cell shared by every iteration.
basedpython prints `1 2 3`. comprehension targets bind the same way. see
[unique loop bindings](unique-loop-bindings.md)

### imports are lazy by default

every `import` and `from ... import` in a `.by` file is marked lazy, so the
module's body does not execute until something first touches it:

```by
import os

print(os)   # this is what loads it
```

an import with a side effect — registering a plugin, patching something at
module scope — no longer happens just because the importing module was loaded.
`from __future__ import ...`, `from x import *`, and an unaliased `import a.b`
stay eager. see [lazy imports](lazy-imports.md)

## what an annotation means

the same annotation denotes a different type

### a string is a string type, not a forward reference

```by
x: "Foo"
```

python reads that as a deferred reference to the name `Foo`; basedpython reads
it as the string-literal type. there is no manual forward-reference syntax
because none is needed — the transpiler
[quotes a self-reference for you](forward-references.md) when the
runtime requires it

### `float` means `float`

python's typing spec special-cases `float` to mean `int | float`, and `complex`
to mean `int | float | complex`. basedpython does not:

```by
def takes(x: float) -> None: ...

takes(1)   # rejected
```

see [strict `float` and `complex`](no-number-promotions.md)

### `class A[**Kwargs]` is keyword type arguments, not a parameter specification

python's typing denotes that `**P` is a parameter specification, but basedpython
generalises the concept as an upper bound of a standard type parameter:

```by
def f[P: (*: *, **: *)](fn: (**P) -> None): ...

f[(int, str, foo: bool)]

class HasKeywords[**Kwargs]

HasKeywords[foo=int, bar=str]
```

see [generics](generics.md)

### a parameter specification is forwarded with stars

python names a parameter specification's two halves as attributes of the type variable,
`*args: P.args` and `**kwargs: P.kwargs`. basedpython unpacks them the way it unpacks every
other pack, and the attribute spelling is an error:

```by
def deco[P: (*: *, **: *), R](fn: (**P) -> R) -> (**P) -> R:
    def inner(*args: *P, **kwargs: **P) -> R:
        return fn(*args, **kwargs)

    return inner
```

see [forwarding](generics.md#forwarding)

## type checking

the code runs the same; the checker's verdict differs

### an unsolved type variable is `Never`

where a type variable is never solved, python's checkers infer `Unknown` and
stop checking. basedpython infers `Never` in covariant and bivariant positions,
which keeps checking. see
[precise unsolved type variables](precise-unsolved-typevars.md)

### inference is sound where python's is gradual

basedpython infers a precise type in places the spec allows a gradual one, so
code that leaned on `Any` flowing through silently now reports. see
[sound types](sound-types.md)

### the stdlib is typed differently

[typeshed improvements](typeshed.md) lists the whole set. the ones most
likely to report on existing code:

- an optional `re` capture group is `str | None`, not `Any`, so
    `m.group(1).upper()` is an error
- a `functools.cache`d function keeps its parameter list, so a wrong-arity call
    to it is an error
- `dict` / `set` keys are bounded by `Hashable`
- a membership test checks that the operands
    [overlap](overlapping.md), so `"a" in [1, 2]` is an error rather
    than a guaranteed `False`
