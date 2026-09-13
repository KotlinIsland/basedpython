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

## Concatenation for variable-length tuples

Concatenating onto a variable-length tuple keeps the elements whose positions are known from either
end, so the fixed elements before and after the variable-length part survive.

```py
def _(x: tuple[int, ...], y: tuple[str, ...]):
    reveal_type((1, 2) + x)  # revealed: tuple[Literal[1], Literal[2], *tuple[int, ...]]
    reveal_type(x + (3, 4))  # revealed: tuple[*tuple[int, ...], Literal[3], Literal[4]]
    reveal_type((1, 2) + x + (3, 4))  # revealed: tuple[Literal[1], Literal[2], *tuple[int, ...], Literal[3], Literal[4]]
```

A tuple has only one variable-length part, so joining two of them folds both, along with the
elements between them, into a single homogeneous part.

```py
def _(x: tuple[int, ...], y: tuple[str, ...]):
    reveal_type(x + y)  # revealed: tuple[int | str, ...]
    reveal_type((1, 2) + y + (3, 4) + x)  # revealed: tuple[Literal[1], Literal[2], *tuple[int | str, ...]]
```

Joining a variable-length part to itself needs no widening: `x + x` has exactly the values of `x`.

```py
def _(x: tuple[int, ...]):
    reveal_type(x + x)  # revealed: tuple[int, ...]
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
    reveal_type(one_two + x)  # revealed: tuple[Literal[1], Literal[2], *tuple[int, ...]]
    reveal_type(x + three_four)  # revealed: tuple[*tuple[int, ...], Literal[3], Literal[4]]
    reveal_type(one_two + x + three_four)  # revealed: tuple[Literal[1], Literal[2], *tuple[int, ...], Literal[3], Literal[4]]
    reveal_type(one_two + y + three_four + x)  # revealed: tuple[Literal[1], Literal[2], *tuple[int | str, ...]]
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

A non-literal factor repeats the tuple an unknown number of times. The number of elements is
unknown, but their order is not: `(1, "a") * n` always alternates `1` and `"a"`. Python has no
spelling for that type, so it is displayed as the multiplication that produces it.

```py
def _(n: int):
    reveal_type((1, "a") * n)  # revealed: tuple[Literal[1], Literal["a"]] * int
    reveal_type(n * (1, "a"))  # revealed: tuple[Literal[1], Literal["a"]] * int
```

A repeated single element is a homogeneous tuple.

```py
def _(n: int):
    reveal_type((0,) * n)  # revealed: tuple[Literal[0], ...]
```

A factor that would produce a tuple longer than the folding limit keeps the repeated order rather
than spelling out every element.

```py
reveal_type((0,) * 1000)  # revealed: tuple[Literal[0], ...]
reveal_type((0, "a") * 1000)  # revealed: tuple[Literal[0], Literal["a"]] * int
```

Repeating a tuple that is already a repetition, with no elements before or after it, has the same
values as the tuple itself.

```py
def _(x: tuple[int, ...], n: int):
    reveal_type(x * 3)  # revealed: tuple[int, ...]
    reveal_type(x * n)  # revealed: tuple[int, ...]
    reveal_type((1, "a") * n * n)  # revealed: tuple[Literal[1], Literal["a"]] * int
```

When the tuple has elements before or after its variable-length part, those elements would
interleave with the repetitions, so the result falls back to typeshed's `tuple.__mul__`.

```py
def _(x: tuple[int, ...], n: int):
    reveal_type((("a",) + x) * n)  # revealed: tuple[Literal["a"] | int, ...]
```

The fold only applies when the multiplication reaches `tuple`'s own `__mul__` or `__rmul__`. A
factor that `tuple.__mul__` rejects is reported as usual.

```py
def _(s: str):
    # error: [unsupported-operator]
    (1, "a") * s
```
