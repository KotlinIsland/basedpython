# Binary operations on tuples

## Concatenation for heterogeneous tuples

Concatenating fixed-length tuples preserves the type and order of their elements. An empty tuple
contributes no elements.

This inference is not strictly sound: a value annotated as `tuple` can be an instance of a subclass.
We assume such subclasses do not override `__add__` or `__radd__` with behavior incompatible with
ordinary tuple concatenation.

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

## Union operands

Concatenation preserves the possible lengths and element types of each tuple in a union.

```py
def concatenate(left: tuple[int] | tuple[str, str], right: tuple[bytes]) -> None:
    reveal_type(left + right)  # revealed: tuple[int, bytes] | tuple[str, str, bytes]
    reveal_type(right + left)  # revealed: tuple[bytes, int] | tuple[bytes, str, str]
```

## Tuple subclass overrides

An explicitly known tuple subclass can override addition. Its `__add__` return annotation determines
the result. On the right, its `__radd__` can take precedence over the left operand's `__add__`; both
results remain possible because a `tuple` annotation also admits subclass instances.

```py
class Custom(tuple[int]):
    def __add__(self, other: object) -> tuple[int, int]:
        return (1, 2)

    def __radd__(self, other: object) -> str:
        return "custom"

def add_custom(custom: Custom, plain: tuple[str]) -> None:
    reveal_type(custom + plain)  # revealed: tuple[int, int]
    reveal_type(plain + custom)  # revealed: str | tuple[str | int, ...]
```

## Many conditional concatenations

Independent conditions can produce exponentially many tuple shapes. For large combinations,
concatenation approximates the result as a variable-length tuple to keep inference bounded. Other
union alternatives still use their operator methods.

```py
def flag() -> bool:
    return True

class Custom(tuple[str]):
    def __add__(self, other: object) -> "Custom":
        return self

def initial() -> Custom | tuple[()]:
    return ()

value = ()
mixed = initial()
if flag():
    value += (1,)
    mixed = mixed + (1,)
if flag():
    value += (2,)
    mixed = mixed + (2,)
if flag():
    value += (3,)
    mixed = mixed + (3,)
if flag():
    value += (4,)
    mixed = mixed + (4,)
if flag():
    value += (5,)
    mixed = mixed + (5,)
if flag():
    value += (6,)
    mixed = mixed + (6,)

reveal_type(value)  # revealed: tuple[Literal[6, 1, 2, 3, 4, 5], ...]
reveal_type(mixed)  # revealed: Custom | tuple[Literal[6, 1, 2, 3, 4, 5], ...]
```

## Concatenating two large unions

Each operand has six tuple shapes, so concatenation can produce 36 combinations. Both ordinary and
augmented assignment use a variable-length approximation when combining these alternatives.

```py
Choice = (
    tuple[int, int, str]
    | tuple[int, str, int]
    | tuple[str, int, int]
    | tuple[int, str, str]
    | tuple[str, int, str]
    | tuple[str, str, int]
)

def concatenate(left: Choice, right: Choice) -> None:
    reveal_type(left + right)  # revealed: tuple[int | str, ...]
    result = left
    result += right
    reveal_type(result)  # revealed: tuple[int | str, ...]
```

Non-tuple alternatives still use their addition and reflected addition methods, even when the tuple
combinations require approximation.

```py
class Custom:
    def __add__(self, other: object) -> "Custom":
        return self

    def __radd__(self, other: object) -> bytes:
        return b"custom"

def concatenate_custom(left: Choice | Custom, right: Choice | Custom) -> None:
    reveal_type(left + right)  # revealed: bytes | tuple[int | str, ...] | Custom
    result = left
    result += right
    reveal_type(result)  # revealed: bytes | tuple[int | str, ...] | Custom
```

## Repeated doubling

Doubling a tuple repeatedly grows its length exponentially without creating a union. Very large
concatenations also use a variable-length approximation.

```py
value = (1,)
value = value + value
value = value + value
value = value + value
value = value + value
value = value + value
value = value + value
value = value + value
value = value + value
value = value + value
value = value + value
value = value + value
value = value + value
value = value + value
reveal_type(value)  # revealed: tuple[Literal[1], ...]
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
