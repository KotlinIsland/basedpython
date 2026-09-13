# basedpython: repeated tuples

A tuple repeated by an integer whose value is unknown has an unknown length, but its elements still
come in the order the tuple lists them. `(1, "a") * n` is `()`, `(1, "a")`, `(1, "a", 1, "a")` and
so on, so every even position holds `1` and every odd position holds `"a"`. The type `(A, B) * int`
describes exactly those tuples, spelled as the multiplication that produces them.

```toml
[environment]
python-version = "3.12"
```

## multiplying a tuple by an integer

Multiplying a tuple by an `int` produces a repeated tuple, whichever side the tuple is on.

```by
def f(n: int):
    reveal_type((1, "a") * n)  # revealed: (1, "a") * int
    reveal_type(n * (1, "a"))  # revealed: (1, "a") * int
```

## spelling the type

A repeated tuple type is written as a tuple type multiplied by `int`.

```by
def f(pairs: (int, str) * int):
    reveal_type(pairs)  # revealed: (int, str) * int
```

Fixed elements before or after the repetitions are written around it, with the repeated tuple
unpacked into the outer tuple.

```by
def f(framed: (bytes, *(int, str) * int, bytes)):
    reveal_type(framed)  # revealed: (bytes, *(int, str) * int, bytes)
```

A repeated tuple is a type like any other, so it can be aliased and used as a type argument.

```by
type Pairs = (int, str) * int

def f(pairs: Pairs, nested: list[(int, str) * int]):
    reveal_type(pairs)  # revealed: (int, str) * int
    reveal_type(nested)  # revealed: list[(int, str) * int]
```

## the length of the repeated run

How many elements the run has is part of the type: repeating `(int, str, int, str)` produces lengths
0, 4, 8 and so on, while repeating `(int, str)` also produces 2 and 6. The two are different types,
and the longer run is the more specific one.

```by
def f(quads: (int, str, int, str) * int, pairs: (int, str) * int):
    reveal_type(quads)  # revealed: (int, str, int, str) * int
    shorter_run: (int, str) * int = quads
    # error: [invalid-assignment]
    longer_run: (int, str, int, str) * int = pairs
```

Repeating a run of one element is a homogeneous tuple, and repeating the empty tuple only ever
produces the empty tuple.

```by
def f(ints: (int,) * int, empty: () * int):
    reveal_type(ints)  # revealed: (*: int)
    reveal_type(empty)  # revealed: ()
```

A run of two identical types is not the homogeneous tuple of that type: it always has an even number
of elements.

```by
def f(evens: (int, int) * int):
    reveal_type(evens)  # revealed: (int, int) * int
    homogeneous: (*: int) = evens
    # error: [invalid-assignment]
    one: (int, int) * int = (1,)
    two: (int, int) * int = (1, 2)
```

## indexing

The element at an index is known from the repeated order, from either end.

```by
def f(pairs: (int, str) * int):
    reveal_type(pairs[0])  # revealed: int
    reveal_type(pairs[1])  # revealed: str
    reveal_type(pairs[2])  # revealed: int
    reveal_type(pairs[-1])  # revealed: str
    reveal_type(pairs[-2])  # revealed: int
```

When there are fixed elements before the repetitions, an index past them can land in the repetitions
or, when there are none, on an element after them.

```by
def f(framed: (bytes, *(int, str) * int, bytes)):
    reveal_type(framed[0])  # revealed: bytes
    reveal_type(framed[1])  # revealed: int | bytes
    reveal_type(framed[2])  # revealed: str
    reveal_type(framed[3])  # revealed: int | bytes
    reveal_type(framed[-2])  # revealed: bytes | str
```

## iterating

Iterating a repeated tuple produces any of its elements.

```by
def f(pairs: (int, str) * int):
    for element in pairs:
        reveal_type(element)  # revealed: int | str
```

## unpacking

Unpacking into fixed targets takes whole repetitions, so each target's element is known.

```by
def f(pairs: (int, str) * int):
    # error: [refutable-unpacking]
    first, second = pairs
    reveal_type(first)  # revealed: int
    reveal_type(second)  # revealed: str
```

No number of repetitions has three elements, so unpacking into three targets always fails.

```by
def f(pairs: (int, str) * int):
    # error: [invalid-assignment] "Wrong number of values to unpack: Expected 3"
    a, b, c = pairs
```

A starred target collects the elements between the fixed targets into a list.

```by
def f(pairs: (int, str) * int):
    # error: [refutable-unpacking]
    first, *rest = pairs
    reveal_type(first)  # revealed: int
    reveal_type(rest)  # revealed: list[int | str]
```

A sequence pattern matches the same way, and a pattern with a length that no number of repetitions
has never matches.

```by
def f(pairs: (int, str) * int):
    match pairs:
        case (first, second):
            reveal_type(first)  # revealed: int
            reveal_type(second)  # revealed: str
        case (first, second, third):
            reveal_type(third)  # revealed: Never
```

## narrowing by length

A length check narrows a repeated tuple to the fixed-length tuple with that many elements, or to
`Never` when no number of repetitions has that length.

```by
def f(pairs: (int, str) * int):
    if len(pairs) == 4:
        reveal_type(pairs)  # revealed: (int, str, int, str)
    if len(pairs) == 3:
        reveal_type(pairs)  # revealed: Never
```

## concatenation

Concatenating a fixed-length tuple keeps its elements in place around the repetitions.

```by
def f(pairs: (int, str) * int, end: (bytes,)):
    reveal_type(pairs + end)  # revealed: (*(int, str) * int, bytes)
    reveal_type(end + pairs)  # revealed: (bytes, *(int, str) * int)
```

Concatenating a repeated tuple with itself repeats the same unit.

```by
def f(pairs: (int, str) * int):
    reveal_type(pairs + pairs)  # revealed: (int, str) * int
```

Elements between two of the same repetitions join the run when they are a whole number of it, since
every length the two runs reach together is one run's own length plus those elements.

```by
def f(pairs: (int, str) * int, one_run: (int, str), part: (int,)):
    reveal_type(pairs + one_run + pairs)  # revealed: (int, str, *(int, str) * int)
    reveal_type(pairs + part + pairs)  # revealed: (*: int | str)
```

Two different repetitions cannot share one repeated part, so their concatenation is a homogeneous
tuple of every element either one has.

```by
def f(pairs: (int, str) * int, bytes_: (*: bytes)):
    reveal_type(pairs + bytes_)  # revealed: (*: int | str | bytes)
```

## assignability

A fixed-length tuple is assignable to a repeated tuple when it is a whole number of repetitions of
compatible elements.

```by
def f():
    empty: (int, str) * int = ()
    two: (int, str) * int = (1, "a", 2, "b")
    # error: [invalid-assignment]
    three: (int, str) * int = (1, "a", 2)
    # error: [invalid-assignment]
    swapped: (int, str) * int = ("a", 1)
```

A repeated tuple is assignable to a homogeneous tuple of its elements, but not to a fixed-length
tuple, since it can have any whole number of repetitions.

```by
def f(pairs: (int, str) * int):
    homogeneous: (*: int | str) = pairs
    # error: [invalid-assignment]
    fixed: (int, str) = pairs
```

A homogeneous tuple is not assignable to a repeated tuple, since its elements can come in any order
and number.

```by
def f(elements: (*: int | str)):
    # error: [invalid-assignment]
    pairs: (int, str) * int = elements
```

That holds however wide the elements are: a tuple whose length is any number at all is not one whose
length is a multiple of two.

```by
def f(elements: (*: int | str)):
    # error: [invalid-assignment]
    pairs: (int | str, int | str) * int = elements
```

A repeated tuple is assignable to another when its run is a whole number of the other's runs, with
compatible elements in the same positions.

```by
def f(pairs: (int, str) * int, quads: (int, str, bool, str) * int):
    wider: (int | bytes, str) * int = pairs
    shorter_run: (int, str) * int = quads
    # error: [invalid-assignment]
    rotated: (str, int) * int = pairs
    # error: [invalid-assignment]
    longer_run: (int, str, bool, str) * int = pairs
```

Fixed elements around the repetitions line up with the target's, even when the two tuples split the
same elements differently between the fixed part and the repetitions.

```by
def f(framed: (int, *(str, int) * int)):
    same: (*(int, str) * int, int) = framed
    # error: [invalid-assignment]
    different: (*(str, int) * int, int) = framed
```

A tuple of unknown length and element type is assignable to a repeated tuple, as it is to any other
tuple.

```by
from typing import Any

def f(anything: (*: Any)):
    pairs: (int, str) * int = anything
```

## slicing

Reversing a repeated tuple reverses the order of its run, including any fixed elements around it.

```by
def f(pairs: (int, str) * int, framed: (bytes, *(int, str) * int, bytes)):
    reveal_type(pairs[::-1])  # revealed: (str, int) * int
    reveal_type(framed[::-1])  # revealed: (bytes, *(str, int) * int, bytes)
```

Cutting whole fixed elements off either end keeps the repetitions.

```by
def f(framed: (bytes, *(int, str) * int, bytes)):
    reveal_type(framed[1:])  # revealed: (*(int, str) * int, bytes)
```

A slice that starts inside the repetitions has no repeated order left to describe: the elements it
keeps depend on how many repetitions there are.

```by
def f(pairs: (int, str) * int):
    reveal_type(pairs[2:])  # revealed: (*: int | str)
```

## variadic parameters

A variadic parameter annotated with an unpacked repeated tuple accepts whole repetitions of
arguments.

```by
def f(*args: *((int, str) * int)):
    reveal_type(args)  # revealed: (int, str) * int

f()
f(1, "a")
f(1, "a", 2, "b")
```

Arguments that leave a repetition half filled are reported against the parameter as a whole, while
an argument of the wrong type is reported against that argument.

```by
# error: [invalid-argument-type] "Arguments to `*args` of function `f` do not fill whole repetitions"
f(1, "a", 2)
# error: [invalid-argument-type] "Argument to function `f` is incorrect: Expected `int`, found `"a"`"
# error: [invalid-argument-type] "Argument to function `f` is incorrect: Expected `str`, found `1`"
f("a", 1)
```

A splatted argument is checked against the whole tuple too. Its elements have no known positions of
their own, so only a tuple that fills whole repetitions is accepted.

```by
def splats(strings: (*: str), pairs: (int, str) * int, rotated: (str, int) * int):
    f(*pairs)
    f(*strings)  # error: [invalid-argument-type]
    f(*rotated)  # error: [invalid-argument-type]
    f(1, *strings)  # error: [invalid-argument-type]
```

## callable and protocol parameters

A function is only accepted where a repeated variadic is expected when it accepts every call that
parameter promises.

```by
from typing import Protocol

class Pairs(Protocol):
    def __call__(self, *args: *((int, str) * int)) -> None: ...

def anything(*args: object) -> None: ...
def elements(*args: int | str) -> None: ...
def same(*args: *((int, str) * int)) -> None: ...
def strings(*args: str) -> None: ...
def rotated(*args: *((str, int) * int)) -> None: ...
def two(a: int, b: str) -> None: ...
def tuples(*args: (*: int | str)) -> None: ...

def f():
    a: Pairs = anything
    b: Pairs = elements
    c: Pairs = same
    # error: [invalid-assignment]
    d: Pairs = strings
    # error: [invalid-assignment]
    e: Pairs = rotated
    # a call of two arguments is one repetition, but four is not two calls
    # error: [invalid-assignment]
    g: Pairs = two
    # each argument is a tuple here, not an element of one
    # error: [invalid-assignment]
    h: Pairs = tuples
```

## generic functions

A type parameter in a repeated run is solved from the elements at the same positions in the
argument, whether the argument repeats them or spells them out.

```by
def pair_up[T, U](values: (T, U) * int) -> (U, T) * int:
    return values[1], values[0]

def f(n: int, quads: (int, str, bool, bytes) * int):
    reveal_type(pair_up((1, "a") * n))  # revealed: ("a", 1) * int
    reveal_type(pair_up(("a", 1)))  # revealed: (1, "a") * int
    reveal_type(pair_up(quads))  # revealed: (str | bytes, int) * int
```

An argument whose length is not a whole number of the run's repetitions does not match.

```by
def g():
    pair_up(("a", 1, "b"))  # error: [invalid-argument-type]
```
