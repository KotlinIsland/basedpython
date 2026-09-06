# Bounds that name another type parameter

```toml
[environment]
python-version = "3.13"
```

A type parameter's bound may name a type parameter that is already in scope where the bound is
written. PEP 695 states the rule: "The bound for a type parameter may reference other type
parameters defined in the same list, but forward references are not allowed."

## The bound holds inside the body

`R` is bounded by `T`, so a value of type `R` is a value of type `T`.

```py
def pick[T, R: T](t: T, r: R) -> T:
    return r
```

The other direction does not hold: `T` is not bounded by `R`.

```py
def wrong[T, R: T](t: T, r: R) -> R:
    return t  # error: [invalid-return-type]
```

A bound can name a parameter through a generic class, which is PEP 695's own example. The members of
`T` are the members of `Sequence[S]`, and reading one through `T` gives back `S`.

```py
from typing import Sequence

class Pair[S, T: Sequence[S]]:
    def first(self, t: T) -> S:
        return t[0]

    def count(self, t: T) -> int:
        return len(t)
```

## A method's bound can name its class's type parameter

`T` belongs to the class's list, which encloses the method's, so it is in scope in the method's
bound. Binding the receiver decides what it means.

```py
class Owner[T]:
    def narrow[U: T](self, u: U) -> T:
        return u

reveal_type(Owner[int]().narrow(1))  # revealed: int

def _(owner: Owner[object]) -> None:
    reveal_type(owner.narrow("a"))  # revealed: object
```

## Explicit specialization

The bound is checked against the argument with the arguments already chosen substituted into it, so
the type it is measured against is `Sequence[int]` rather than `Sequence[S]`.

```py
from typing import Sequence

class Pair[S, T: Sequence[S]]:
    x: T

def _(ok: Pair[int, list[int]]) -> None:
    reveal_type(ok.x)  # revealed: list[int]

# error: [invalid-type-arguments] "Type `list[str]` is not assignable to upper bound `Sequence[int]` of type variable `T@Pair`"
def _(bad: Pair[int, list[str]]) -> None: ...
```

A parameter left to its default counts as chosen, so a later bound naming it sees the default.
Whether the default itself sits inside such a bound is a question about a specialization too, so it
is asked here rather than at the declaration.

```py
from typing import Sequence

class Defaulted[S = int, T: Sequence[S] = list[int]]:
    x: T

def _(ok: Defaulted[int, list[int]]) -> None:
    reveal_type(ok.x)  # revealed: list[int]

# error: [invalid-type-arguments] "Type `list[str]` is not assignable to upper bound `Sequence[int]` of type variable `T@Defaulted`"
def _(bad: Defaulted[int, list[str]]) -> None: ...
```

A type alias declares its type parameters the same way.

```py
from typing import Sequence

type Named[S, T: Sequence[S]] = tuple[S, T]

def _(ok: Named[int, list[int]]) -> None:
    reveal_type(ok)  # revealed: tuple[int, list[int]]

# error: [invalid-type-arguments] "Type `list[str]` is not assignable to upper bound `Sequence[int]` of type variable `T@Named`"
def _(bad: Named[int, list[str]]) -> None: ...
```

## Explicit specialization written by name

basedpython lets a subscript name the type parameter it fills. A bound does not stop applying
because the argument that fills it was written by name.

```by
from typing import Sequence

class Pair[S, T: Sequence[S]]:
    x: T

# error: [invalid-type-arguments] "Type `list[str]` is not assignable to upper bound `Sequence[int]` of type variable `T@Pair`"
def _(bad: Pair[S=int, T=list[str]]) -> None: ...
```

## Inference

The bound is a relation between the two type parameters, so it takes part in solving the call rather
than being checked against one parameter at a time. `R`'s solution is a floor under `T`.

```py
class Animal: ...
class Dog(Animal): ...

def pick[T, R: T](t: T, r: R) -> T:
    return t

reveal_type(pick(Dog(), Dog()))  # revealed: Dog
reveal_type(pick(Animal(), Dog()))  # revealed: Animal
```

When the argument for `r` is wider than the argument for `t`, the relation is still satisfiable —
`T` widens to accommodate it. This is the same answer Java and TypeScript give, and it is the only
answer that is total: the alternative, solving `T` from `t` alone and then checking `R` against it,
would have to report an error here even though `T = Animal` satisfies every constraint.

```py
class Animal: ...
class Dog(Animal): ...

def pick[T, R: T](t: T, r: R) -> T:
    return t

reveal_type(pick(Dog(), Animal()))  # revealed: Animal
```

Widening is also what lets a type parameter be found through the bound alone. Nothing but the bound
mentions `T` here, so without the relation it would have no solution at all.

```py
def only_bound[T, R: T](r: R) -> T:
    return r

reveal_type(only_bound(1))  # revealed: Literal[1]
```

The relation is transitive: `Q` is bounded by `R`, which is bounded by `T`, so the one argument here
reaches `T` through two hops.

```py
def chain[T, R: T, Q: R](q: Q) -> T:
    return q

reveal_type(chain(1))  # revealed: Literal[1]
```

A ceiling at the far end of the chain reaches back down it, however many hops long it is.

```py
def capped_chain[T: int, R: T, Q: R](q: Q) -> None: ...

capped_chain(1)
# error: [invalid-argument-type] "Argument type `Literal["s"]` does not satisfy upper bound `int` of type variable `Q`"
capped_chain("s")
```

A named parameter that is constrained rather than bounded caps the one that names it at the union of
its constraints.

```py
def constrained[T: (int, str), R: T](r: R) -> None: ...

constrained(1)
# error: [invalid-argument-type] "Argument type `float` does not satisfy upper bound `int | str` of type variable `R`"
constrained(1.5)
```

## An argument that cannot satisfy the relation

Widening is only available while nothing else pins the type parameter. An invariant occurrence pins
it, and then the relation is decided rather than accommodated.

```py
def invariant[T, R: T](t: list[T], r: R) -> None: ...

invariant([1], 2)
# error: [invalid-argument-type] "Argument type `Literal["s"]` does not satisfy upper bound `int` of type variable `R`"
invariant([1], "s")
```

A ceiling on the parameter that is named puts the same ceiling on the parameter that names it.

```py
def capped[T: int, R: T](r: R) -> None: ...

capped(3)
# error: [invalid-argument-type] "Argument type `Literal["s"]` does not satisfy upper bound `int` of type variable `R`"
capped("s")
```

Several arguments can fill the same type parameter, and the one reported is the one that is actually
outside the bound.

```py
def two[T, R: T](t: list[T], first: R, second: R) -> None: ...

# error: [invalid-argument-type] "Argument type `Literal["s"]` does not satisfy upper bound `int` of type variable `R`"
two([1], 2, "s")
```

## Names a bound may not use

A parameter is not in scope inside its own bound: there is nothing for the reference to resolve to.

```py
# error: [invalid-type-variable-bound] "TypeVar upper bound cannot reference the type parameter it bounds"
def f[T: list[T]](x: T) -> T:
    return x
```

A later parameter is not in scope yet.

```py
# error: [invalid-type-variable-bound] "TypeVar upper bound cannot reference later type parameter `T`"
def g[S: T, T](s: S, t: T) -> None: ...
```

A legacy `TypeVar` is declared by an assignment, so it holds no position in a list for the bound to
be after — and one `TypeVar` object can be reused by two unrelated generics, so naming it in a bound
names nothing in particular.

```py
from typing import TypeVar

S = TypeVar("S")

def h[T: S](x: T) -> T:  # error: [invalid-type-variable-bound] "TypeVar upper bound cannot be generic"
    return x
```

Only a method's own list may name its class's. A nested class or a nested function declares a list
that nothing substitutes into — the enclosing parameter would still be standing there at every use
of the generic — so those bounds are rejected where they are written and the generic stays usable.

```py
class Outer[T]:
    # error: [invalid-type-variable-bound] "TypeVar upper bound cannot be generic"
    class Inner[U: T]: ...

def _(inner: Outer.Inner[int]) -> None: ...
def outer[T](t: T) -> None:
    # error: [invalid-type-variable-bound] "TypeVar upper bound cannot be generic"
    def inner[U: T](u: U) -> U:
        return u

    inner(1)
```

Constraints are not bounds. A constrained type parameter takes its solution *from* its constraint
set, and a type parameter does not name a type to take.

```py
# error: [invalid-type-variable-constraints] "TypeVar constraint cannot be generic"
def i[S, T: (S, int)](s: S, t: T) -> None: ...
```

## A rejected bound is discarded, not merely reported

Two parameters that bound each other have no grounding, and several parts of the type system reduce
a type parameter to its bound by recursion. Reporting a cycle without also dropping it would run
that recursion forever, so a rejected bound leaves the parameter unbounded.

```py
# error: [invalid-type-variable-bound] "TypeVar upper bound cannot reference later type parameter `R`"
def mutual[T: R, R: T](t: T, r: R) -> None:
    reveal_type(t)  # revealed: T@mutual
    reveal_type(r)  # revealed: R@mutual

mutual(1, "a")
```

## `Self` is not one of these names

`Self` is bound by the enclosing class rather than by the list being declared, so a bound naming it
is substituted when the method binds its receiver, and a bound naming both is fine.

```py
from typing import Self

class C:
    def clone[T: Self](self, other: T) -> T:
        return other

class Sub(C): ...

def _(s: Sub) -> None:
    reveal_type(s.clone(s))  # revealed: Sub
```

## A variadic pack's bound

A pack's bound describes its members rather than the pack's own value, so it is checked member by
member and has nowhere to record a relation between two parameters.

```by
# error: [invalid-type-variable-bound] "A variadic pack's bound cannot be generic"
def pack[T, *Ts: T](t: T, *ts: *Ts) -> None: ...
```
