# Binary operations on tuples

## Concatenation for heterogeneous tuples

Concatenating two fixed-length tuples folds into a fixed-length tuple that preserves the exact
element order and count, rather than widening to `tuple[T, ...]`.

```py
reveal_type((1, 2) + (3, 4))  # revealed: tuple[Literal[1], Literal[2], Literal[3], Literal[4]]
reveal_type(() + (1, 2))  # revealed: tuple[Literal[1], Literal[2]]
reveal_type((1, 2) + ())  # revealed: tuple[Literal[1], Literal[2]]
reveal_type(() + ())  # revealed: tuple[()]

def _(x: tuple[int, str], y: tuple[None, tuple[int]]):
    reveal_type(x + y)  # revealed: tuple[int, str, None, tuple[int]]
    reveal_type(y + x)  # revealed: tuple[None, tuple[int], int, str]
```

## Concatenation for homogeneous tuples

```py
def _(x: tuple[int, ...], y: tuple[str, ...]):
    reveal_type(x + x)  # revealed: tuple[int, ...]
    reveal_type(x + y)  # revealed: tuple[int | str, ...]
    reveal_type((1, 2) + x)  # revealed: tuple[int, ...]
    reveal_type(x + (3, 4))  # revealed: tuple[int, ...]
    reveal_type((1, 2) + x + (3, 4))  # revealed: tuple[int, ...]
    reveal_type((1, 2) + y + (3, 4) + x)  # revealed: tuple[int | str, ...]
```

We get the same results even when we use a legacy type alias, even though this involves first
inferring the `tuple[...]` expression as a value form. (Doing so gives a generic alias of the
`tuple` type, but as a special case, we include the full detailed tuple element specification in
specializations of `tuple`.)

```py
from typing import Literal

OneTwo = tuple[Literal[1], Literal[2]]
ThreeFour = tuple[Literal[3], Literal[4]]
IntTuple = tuple[int, ...]
StrTuple = tuple[str, ...]

def _(one_two: OneTwo, x: IntTuple, y: StrTuple, three_four: ThreeFour):
    reveal_type(x + x)  # revealed: tuple[int, ...]
    reveal_type(x + y)  # revealed: tuple[int | str, ...]
    reveal_type(one_two + x)  # revealed: tuple[int, ...]
    reveal_type(x + three_four)  # revealed: tuple[int, ...]
    reveal_type(one_two + x + three_four)  # revealed: tuple[int, ...]
    reveal_type(one_two + y + three_four + x)  # revealed: tuple[int | str, ...]
```

## Repetition for heterogeneous tuples

Multiplying a fixed-length tuple by a literal integer folds into a fixed-length tuple whose elements
are repeated, matching the runtime behaviour of `tuple.__mul__`. Repetition is commutative, and a
`bool` factor is treated as `0` or `1`.

```py
reveal_type((1, "a") * 3)  # revealed: tuple[Literal[1], Literal["a"], Literal[1], Literal["a"], Literal[1], Literal["a"]]
reveal_type(3 * (1, "a"))  # revealed: tuple[Literal[1], Literal["a"], Literal[1], Literal["a"], Literal[1], Literal["a"]]
reveal_type((1, "a") * True)  # revealed: tuple[Literal[1], Literal["a"]]
```

A non-positive factor folds to the empty tuple.

```py
reveal_type((1, "a") * 0)  # revealed: tuple[()]
reveal_type((1, "a") * -2)  # revealed: tuple[()]
```

A non-literal factor, or a factor that would produce a tuple longer than the folding limit, falls
back to typeshed's `tuple.__mul__` (which widens to `tuple[T, ...]`).

```py
def _(n: int):
    reveal_type((1, "a") * n)  # revealed: tuple[Literal[1, "a"], ...]
    reveal_type((0,) * 1000)  # revealed: tuple[Literal[0], ...]
```

Homogeneous (variable-length) tuples are also left to `tuple.__mul__`.

```py
def _(x: tuple[int, ...]):
    reveal_type(x * 3)  # revealed: tuple[int, ...]
```
