# parametric type tests

`isinstance(x, int)`). when the right-hand side is a *parameterized* generic,
`x is C[args]` means `type(x) <: C[args]`, so it follows `C`'s
[variance](variance.md):

```by
class Box[out T]:
    def __init__(self): ...

b: Box[int] = Box[int]()
b is Box[object]   # True — Box is covariant, and int <: object
```

## why `isinstance` can't answer this

plain `isinstance` cannot answer a parameterized test — `x is list[int]`:
`isinstance(x, list[int])` is a runtime `TypeError`, and the builtins erase
their type arguments anyway. basedpython resolves it from static types at
compile time, keeping a runtime residue only where one is needed

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

- **undecidable, protocol target → structural check**. a protocol's instances
    record no specialization, but basedpython reifies class attribute
    annotations, so the value's class is inspected member by member — each
    reified annotation against the member's specialized type, by that member's
    variance. a method member is checked the same way through its parameter and
    return annotations. see [below](#a-protocol-target-is-checked-structurally)

- **erased union parameter → the call site answers**. a parameter typed as a
    union of specializations of one erased origin (`list[int] | list[str]`)
    cannot be discriminated by looking at the value — both arms are the same
    C-level `list`. the missing fact is not a property of the value but of the
    *binding*, so it travels with the call: the parameter is given a reified
    type parameter and the test compares that cell. see
    [below](#an-erased-union-parameter-carries-its-specialization)

- **undecidable, builtin target → error**. any other runtime `list` / `dict` /
    `set` / `tuple` carries no record of its type arguments, so it cannot be
    checked at runtime. this is an
    [`erased-type-check`](#the-erased-type-check-error) error — there is
    deliberately no "check the first element" heuristic, since an empty
    collection has no element and a builtin's element type is erased

so of the two undecidable cases, only the *target* distinguishes them:

```by
class A[T]: ...

def f(a: object):
    a is A[int]      # valid — A's instances carry __orig_class__
    a is list[int]   # error — a runtime list erases its element type
```

## the target may be spelled through an alias

The target need not be a literal subscript. A name bound to a specialization, or a PEP 695 `type`
alias for one, resolves the same way — `x is X` behaves exactly like `x is A[int]`:

```by
class A[T]: ...

X = A[int]          # implicit alias
type Y = A[int]     # PEP 695 alias

def f(a: object):
    a is X          # same as `a is A[int]`
    a is Y          # same as `a is A[int]`
```

A bare class name that is *not* a specialization (`a is int`) stays an ordinary instance test. One
limitation: the [reified-cell](#how-a-test-resolves) path spells `T == <arg>` from the target's
syntax, so it needs a literal subscript — `x: T is X` through an alias falls back to the fold or the
probe rather than the cell comparison.

## variance

both the fold and the probe read the *effective* variance of each type
argument: the declared variance combined with any [use-site
projection](variance.md#use-site-variance) the target spells. so an invariant
`T` can be matched covariantly for one test by asking for it:

```by
class A[in out T]:
    def __init__(self): ...

def f(a: A[*]):
    print(a is A[out int])   # True for an A[bool] — `out` projects T covariantly
    print(a is A[int])       # False for an A[bool] — T is invariant unprojected
```

this is the same combination that decides assignability, so a test can never
contradict the annotation: wherever `b: A[out int] = a` is accepted,
`a is A[out int]` is `True`.

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
`is not` keyword form is a type test

```by
xs = [1, 2]
xs === list[int]   # plain identity — the list is not the class object
```

## requirements

a user-generic probe works on any target version. the reified-cell token
equality path is only reached inside a [reified generic](reified-generics.md),
which already requires python 3.12+

## a protocol target is checked structurally

a protocol has no identity to record, so there is nothing on the value saying
which specialization it satisfies. what there *is* is the value's class, whose
annotations basedpython reifies — so the check is done member by member against
those, each by its own variance:

```by
from typing import Protocol

class HasA[T](Protocol):
    a: T

class IntAttr:
    a: int

def f(x: object):
    x is HasA[int]      # True for an `IntAttr`
```

an invariant member is exact and a covariant one follows subtyping, the same way
the target's own variance decides every other parametric test. a method member is
checked through its annotations too — parameters contravariantly, the return
covariantly.

### only a class-level annotation survives

python records nothing for a `self.a: int` written inside `__init__`, so a member
declared there cannot be read back at runtime. the checker still accepts such a
class as satisfying the protocol, so answering `False` would put the two halves
in contradiction — the test raises instead:

```by
class InitLevel:
    def __init__(self) -> None:
        self.a: int = 1

InitLevel() is HasA[int]    # TypeError: its type is declared inside a method
```

declare the member in the class body and the check answers. this is the same
line the erased builtin target takes: refuse, never guess.

### the cast forms usually never get here

a [checked cast](checked-cast.md) shares this engine, but only for a value the
checker could not already decide. `InitLevel() cast? HasA[int]` is decided
*statically* — ty accepts that class as satisfying the protocol — so the cast
folds to a pass-through and no runtime check is emitted at all. the `is` above
raises only because its value is an `object`, where nothing is known statically
and the residue has to run.

so the two can differ on the same class: the cast succeeds, the test refuses.
both are right for what they were asked, but it is worth knowing the cast is not
re-verifying what the checker already concluded.

## an erased union parameter carries its specialization

A union of specializations of one erased origin is undecidable at runtime — `list[int]` and
`list[str]` are the same `list`, and a builtin rejects the `__orig_class__` stamp. Probing the value
answered `False` for every arm, which is sound (only the positive branch narrows) but useless:

```by
def f(data: list[int] | list[str]):
    if data is list[int]: ...   # every arm answered False
    if data is list[str]: ...
```

The specialization is a static fact about the binding, known wherever the argument was written. So
it travels with the call instead of being asked of the value: the parameter becomes generic over the
differing argument, and that type parameter is [reified](reified-generics.md).

```by
def f(data: list[int] | list[str]):
    if data is list[int]:       # compares the reified cell
        print("it's ints")
```

This is a lowering detail, not surface syntax. The checker keeps seeing the union you declared —
`reveal_type(data)` is `list[int] | list[str]`, and nothing basedpython reports ever names the
synthesized parameter.

The rewrite is driven by the *signature*, so a function that only forwards its value is rewritten
too and the specialization survives the hop:

```by
def middle(data: list[int] | list[str]):
    f(data)                     # forwards its own cell

middle(list[int]())             # supplies the specialization
```

### when it does not apply

- **below python 3.12.** Reification compiles the type parameter into a PEP 695 closure cell, which
    needs native type-parameter syntax in the output. On an older target the parameter keeps its
    union and the test keeps the probe it always had.
- **a union the runtime can already discriminate.** Different origins (`list[int] | set[int]`) are
    separated by `isinstance`, and a user-defined generic carries `__orig_class__`.
- **more than one differing argument.** `dict[str, int] | dict[bool, str]` varies in two positions,
    so no single type parameter stands for the difference.
- **an argument with no runtime spelling**, such as a scope-local class.

### when the call site cannot supply it

Reaching the function through a value whose type the checker could not pin down — a callable
variable, a dynamic value — leaves nothing to supply the specialization. That raises rather than
guessing:

```text
TypeError: f() cannot tell which specialization it was given: the argument's
type arguments are erased at runtime, and the call site did not record them
```

This is the same line the erased builtin target takes: refuse, never guess.
