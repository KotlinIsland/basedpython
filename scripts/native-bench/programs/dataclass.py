"""`alloc`'s loop over a `@dataclass` instead of a hand-written class

this is `alloc` with one thing changed: the constructor is not in the source.
`alloc` writes its `__init__` out and stores two fields in it; here the field
list is the whole class body and the decorator builds the `__init__` from it at
class-creation time. so the difference between the two rows is what that
synthesised constructor costs against a written one, and `fields` is the floor
under both — the same two stores with no allocation around them

it is worth having as a row because the synthesised constructor is a different
input to the compiler, not a different program: it does not exist in the module
the compiler reads, and whether the backend can see through the decorator to the
fields is exactly what this measures
"""

from dataclasses import dataclass


@dataclass
class Pair:
    x: int
    y: int


def run(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        pair = Pair(i, i + 1)
        total = total + pair.x
        i = i + 1
    return total


def bench() -> int:
    return run(300000)
