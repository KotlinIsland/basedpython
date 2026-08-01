# basedpython: match types

a type alias can pick its value by matching on its type arguments. cases are tried in order, the
first match wins, and a `case` pattern's captures are type variables the body is written in terms
of.

## a shape-indexed nested tuple

the motivating case: an `n`-dimensional tuple built from a shape.

```by
type NDTuple[T, *Shape: int] = match *Shape:
    case ():
        T
    case (Dim, *Rest):
        (NDTuple[T, *Rest],) * Dim


def zero(x: NDTuple[int]):
    reveal_type(x)  # revealed: int


def one(x: NDTuple[int, 3]):
    reveal_type(x)  # revealed: (int, int, int)


def two(x: NDTuple[int, 2, 3]):
    reveal_type(x)  # revealed: ((int, int, int), (int, int, int))
```

## an application inside a generic class

`NDTuple[T, *Shape]` cannot pick a case while `Shape` is a type parameter — the pack could be empty
or not — so it stays symbolic and is decided per construction.

```by
type NDTuple[T, *Shape: int] = match *Shape:
    case ():
        T
    case (Dim, *Rest):
        (NDTuple[T, *Rest],) * Dim


class Array[T, *Shape]:
    init(data: NDTuple[T, *Shape])


def main() -> None:
    a = Array[int, 2, 3](((1, 2, 3), (4, 5, 6)))
    reveal_type(a)  # revealed: final Array[int, 2, 3]
    b = Array[str]("x")
    reveal_type(b)  # revealed: final Array[str]
```

## the shape has to match

```by
type NDTuple[T, *Shape: int] = match *Shape:
    case ():
        T
    case (Dim, *Rest):
        (NDTuple[T, *Rest],) * Dim


class Array[T, *Shape]:
    init(data: NDTuple[T, *Shape])


def main() -> None:
    # error: [invalid-argument-type]
    Array[int, 2, 3](((1, 2), (4, 5, 6)))
```

## a declared member is specialized too

```by
type NDTuple[T, *Shape: int] = match *Shape:
    case ():
        T
    case (Dim, *Rest):
        (NDTuple[T, *Rest],) * Dim


class Array[T, *Shape]:
    data: NDTuple[T, *Shape]

    def get(self) -> NDTuple[T, *Shape]:
        return self.data


def f(a: Array[int, 2, 3]) -> None:
    reveal_type(a.data)  # revealed: ((int, int, int), (int, int, int))
    reveal_type(a.get())  # revealed: ((int, int, int), (int, int, int))
```

## literal patterns

a value pattern matches a literal type exactly, so a shape can be dispatched on.

```by
type Named[*Shape: int] = match *Shape:
    case (1,):
        str
    case (2,):
        bytes
    case _:
        int


def f(a: Named[1], b: Named[2], c: Named[7], d: Named[1, 2]):
    reveal_type(a)  # revealed: str
    reveal_type(b)  # revealed: bytes
    reveal_type(c)  # revealed: int
    reveal_type(d)  # revealed: int
```

## or-patterns and a bare capture

a bare name is a *capture*, exactly as in python's `match` statement — `case X:` matches anything
and binds it, it does not compare against a type called `X`.

```by
type Pick[*Shape: int] = match *Shape:
    case (1,) | (2,):
        bool
    case (X,):
        X
    case _:
        int


def f(a: Pick[1], b: Pick[2], c: Pick[7], d: Pick[3, 4]):
    reveal_type(a)  # revealed: bool
    reveal_type(b)  # revealed: bool
    reveal_type(c)  # revealed: 7
    reveal_type(d)  # revealed: int
```

## a fixed-length pattern needs an exact length

```by
type Pair[*Ts] = match *Ts:
    case (A, B):
        (B, A)


def f(x: Pair[int, str]):
    reveal_type(x)  # revealed: (str, int)
```

## no case matches

an application whose arguments are known but match nothing is an error, rather than silently having
no value.

```by
type Pair[*Ts] = match *Ts:
    case (A, B):
        (B, A)


# error: [invalid-type-arguments] "No `case` of match type `Pair` matches these type arguments"
def f(x: Pair[int]): ...
```

## a capture is scoped to its own case

```by
type Bad[*Ts] = match *Ts:
    case (A,):
        A
    case (B, C):
        # error: [unresolved-reference]
        A
```

## a class pattern has no type-level meaning

```by
type Bad[T] = match T:
    # error: [invalid-type-form] "A match type's `case` cannot use a class or mapping pattern; it matches types, not values"
    case list(x):
        int
    case _:
        str
```

## a guard is not allowed

`case` in a match type decides on the pattern alone.

```by
type Bad[T] = match T:
    # error: [invalid-syntax] "a match type case cannot have a guard; a type-level match decides on the pattern alone"
    case X if X:
        int
    case _:
        str
```

## a case body is a single type expression

```by
type Bad[T] = match T:
    case X:
        # error: [invalid-syntax] "a match type case body must be a single type expression"
        x = 1
    case _:
        str
```

## a `TypeVarTuple` bound applies to every element

```by
type NDTuple[T, *Shape: int] = match *Shape:
    case ():
        T
    case (Dim, *Rest):
        (NDTuple[T, *Rest],) * Dim


# error: [invalid-type-arguments] "Type `"three"` is not assignable to upper bound `int` of type variable tuple `Shape@NDTuple`"
def f(x: NDTuple[int, "three"]): ...
```

## a `TypeVarTuple` bound is not valid in `.py` files

```toml
[environment]
python-version = "3.12"
```

```py
# error: [invalid-syntax] "a bound on a `TypeVarTuple` is a basedpython feature and is not valid in .py files"
type X[*Ts: int] = int
```

## a subject of unknown length is undecidable, not a miss

`()` and `(A, *R)` are both still possible against a variable-length pack, so no case may claim it —
including a later one that would match anything.

```by
type M[*Ts] = match *Ts:
    case ():
        int
    case (A, *R):
        (A,)
    case _:
        bytes


def f(x: M[*tuple[int, ...]]):
    reveal_type(x)  # revealed: Unknown
```

## a gradual subject is undecidable too

```by
from typing import Any


type Pick[T] = match T:
    case 1:
        str
    case X:
        X


def f(x: Pick[Any]):
    reveal_type(x)  # revealed: Unknown
```

## alternatives must bind the same names

python rejects this outright; ruff's parser does not, so it is caught here. left alone, the winning
alternative's missing captures would leak into the alias's value.

```by
type Bad[*Ts] = match *Ts:
    # error: [invalid-type-form] "Alternative patterns of a match type's `case` must all bind the same names; not bound by every alternative: `A`, `B`, `C`"
    case (A,) | (B, C):
        (A, B, C)


def f(x: Bad[int]):
    reveal_type(x)  # revealed: Unknown
```

## a name cannot be captured twice by one pattern

```by
type Dup[*Ts] = match *Ts:
    # error: [invalid-type-form] "Multiple assignments to name `A` in a match type's `case` pattern"
    case (A, A):
        A
    case _:
        int


def f(x: Dup[int, str]):
    reveal_type(x)  # revealed: Unknown
```

## an ill-founded recursion gives up rather than diverging

a case that does not shrink its subject would recurse forever. evaluation gives up once the subject
grows past its budget, instead of taking the checker down with it.

```by
type Grow[*Ts] = match *Ts:
    case ():
        int
    case (A, *R):
        Grow[A, *R, A]


def f(x: Grow[int, str]):
    reveal_type(x)  # revealed: Unknown
```

## a self-referential case is a cycle, not a hang

```by
type Loop[T] = match T:
    case X:
        Loop[X]


def f(x: Loop[int]):
    reveal_type(x)  # revealed: Unknown
```

## `match` is still a name in a plain alias

the alias form only takes over when a `case` block actually follows.

```by
match = int
type M = match


def f(x: M):
    reveal_type(x)  # revealed: int
```
