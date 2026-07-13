# parametric type tests

basedpython's `is` keyword is an instance test (`x is int` lowers to
`isinstance(x, int)`). when the right-hand side is a *parameterized* generic
— `x is list[int]` — plain `isinstance` cannot answer it: `isinstance(x, list[int])` is a runtime `TypeError`, and the builtins erase their type
arguments anyway. `x is C[args]` means `type(x) <: C[args]`, so it follows
`C`'s declared variance. basedpython resolves it rust-style, from static types
at compile time, keeping a runtime residue only where one is needed:

```by
class Box[out T]:
    def __init__(self): ...

b: Box[int] = Box[int]()
b is Box[object]   # True — Box is covariant, and int <: object
```

## how a test resolves

the value's static type decides the strategy:

- **statically provable → folded**. when `type(x)` is a subtype of `C[args]`
    the test folds to `True`; when the two are disjoint, to `False`. this uses
    ty's own subtyping, so it respects variance automatically — a covariant
    `Box[int]` is a `Box[object]`, an invariant `list[object]` is not a
    `list[int]`. side effects in the tested expression are preserved
- **reified type parameter → token equality**. a value typed by a reified
    type parameter carries the specialization in that parameter's runtime cell,
    so the test compares cells: `x: T` against `C[args]` lowers to
    `T == C[args]`; `x: list[T]` against `list[int]` unifies structurally to
    `T == int`. the parameter is [reified](reified-generics.md) for this. this
    works against a builtin target too — the cell holds the alias, nothing is
    erased
- **undecidable, user-generic target → `__orig_class__` probe**. when none of
    the above apply, the last resort reads the value's `__orig_class__` — which
    `A[int](…)` [stamps](type-reification.md) on user generics — and matches
    each type argument by the target's variance. it discriminates a union
    soundly (`x: A[int] | A[str]` against `A[int]`); answers `False` for values
    that carry no `__orig_class__`
- **undecidable, builtin target → error**. a runtime `list` / `dict` / `set`
    / `tuple` carries no record of its type arguments, so it cannot be checked
    at runtime. this is an [`erased-type-check`](#the-erased-type-check-error)
    error — there is deliberately no "check the first element" heuristic, since
    an empty collection has no element and a builtin's element type is erased

so of the two undecidable cases, only the *target* distinguishes them:

```by
class A[T]: ...

def f(a: object):
    a is A[int]      # valid — A's instances carry __orig_class__
    a is list[int]   # error — a runtime list erases its element type
```

## narrowing

the positive branch narrows to the tested specialization. the negative
branch does not narrow — a value that carries no `__orig_class__` answers
`False` even when its static type matches, so the test does not prove the
negation:

```by
class A[T]:
    def __init__(self, t: T):
        self.v: list[T] = [t]

def f(x: A[int] | A[str]):
    if x is A[int]:
        reveal_type(x)  # A[int]
    else:
        reveal_type(x)  # A[int] | A[str]
```

## the erased-type-check error

a parametric test against a builtin collection that isn't statically
decidable is reported under the `erased-type-check` rule:

```by
def f(x):
    return x is list[int]  # error: builtin collections erase their type arguments
```

to make the test work, reify the type parameter (so it compares the cell) or
test against a user-defined generic (whose instances carry `__orig_class__`):

```by
def f[T](x: T) -> bool:
    return x is list[int]   # ok — compares the reified `T`

class A[T]: ...
def g(x) -> bool:
    return x is A[int]      # ok — probes `x.__orig_class__`
```

## `===` is unaffected

the `===` / `!==` identity operators keep python semantics; only the `is` /
`is not` keyword form is a type test.

```by
xs = [1, 2]
xs === list[int]   # plain identity — the list is not the class object
```

## requirements

a user-generic probe works on any target version. the reified-cell token
equality path is only reached inside a [reified generic](reified-generics.md),
which already requires python 3.12+.
