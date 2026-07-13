# parametric type tests

basedpython's `is` keyword is an instance test (`x is int` lowers to
`isinstance(x, int)`). when the right-hand side is a *parameterized* generic
— `x is list[int]` — plain `isinstance` cannot answer it: `isinstance(x, list[int])` is a runtime `TypeError`, and the builtins erase their type
arguments anyway. basedpython resolves the test rust-style, from static types
at compile time, keeping a runtime residue only where one is needed:

```by
xs: list[object] = [1, 2, 3]
xs is list[int]   # False — a list[object] is never a list[int]
```

## how a test resolves

the value's static type decides the strategy:

- **fully known → folded**. the test becomes a constant. arguments are
    invariant and exact, so `list[object]` is never `list[int]`. a value whose
    static type is disjoint from the target (`str is list[int]`) also folds to
    `False`. side effects in the tested expression are preserved
- **reified type parameter → token equality**. a value typed by a reified
    type parameter carries the specialization in that parameter's runtime cell,
    so the test compares cells: `x: T` against `C[args]` lowers to
    `T == C[args]`; `x: list[T]` against `list[int]` unifies structurally to
    `T == int`. the parameter is [reified](reified-generics.md) for this
- **disjoint union → witness**. a union of same-origin specializations whose
    arguments are pairwise disjoint is discriminated by a single witness
    element: `x: list[int] | list[str]` against `list[int]` probes the first
    element's type. an empty collection has no witness and answers `False`
- **anything else → unchecked probe**. the lowering reads the value's
    `__orig_class__` — which `A[int](…)` [stamps](type-reification.md) on user
    generics — and answers `False` when it carries none (every builtin
    collection does). the checker warns here

## narrowing

the positive branch narrows to the tested specialization. the negative
branch does not narrow — an unreified or empty value answers `False` even
when its static type matches, so the test does not prove the negation:

```by
def f(x: list[int] | list[str]):
    if x is list[int]:
        reveal_type(x)  # list[int]
    else:
        reveal_type(x)  # list[int] | list[str]
```

## the unchecked warning

when a test can be verified neither statically nor at runtime, ty warns under
the `unchecked-type-check` rule:

```by
def f(x):
    return x is list[int]  # warning: unchecked type-check
```

the check still runs — at runtime it probes `__orig_class__` and answers
`False` for the builtins, which carry none. to make the test verifiable,
annotate the value or reify the type parameter:

```by
def f[T](x: T) -> bool:
    return x is list[int]   # ok — compares the reified `T`
```

## `===` is unaffected

the `===` / `!==` identity operators keep python semantics; only the `is` /
`is not` keyword form is a type test.

```by
xs = [1, 2]
xs === list[int]   # plain identity — the list is not the class object
```

## requirements

the lowered checks evaluate the target class at runtime, so a token or probe
lowering needs pep 585 (subscriptable builtins), i.e. python 3.9+. below that
a parametric test is a hard transpile error.
