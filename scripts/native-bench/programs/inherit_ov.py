"""an override marked `@override`, reached through a base-typed local

the same dispatch as `inherit`, with the one difference that `Square.area` says
it overrides. `typing.override` only sets `__override__` on the function and
hands it back, so the two rows should cost the same: a gap between them is a
marking decorator changing what gets compiled, which is how idiomatic overrides
once ran interpreted while the census counted them compiled

the mixed list is built by `setup`, as in `inherit`, so only the dispatches are
counted
"""

from typing import override


class Shape:
    def __init__(self, size: int):
        self.size = size

    def area(self) -> int:
        return self.size


class Square(Shape):
    @override
    def area(self) -> int:
        return self.size * self.size


def total(shapes: list[Shape], passes: int) -> int:
    running = 0
    p = 0
    while p < passes:
        i = 0
        while i < len(shapes):
            running = running + shapes[i].area()
            i = i + 1
        p = p + 1
    return running


_shapes: list[Shape] = []


def setup() -> None:
    i = 0
    while i < 200:
        if i % 2 == 0:
            _shapes.append(Shape(i))
        else:
            _shapes.append(Square(i))
        i = i + 1


def bench() -> int:
    return total(_shapes, 300)
