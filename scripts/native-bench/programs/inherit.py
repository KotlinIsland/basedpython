"""an override reached through a base-typed local

the call site cannot know which body it runs, so the direct call `methods`
measures is not available to it. every other object benchmark here has exactly
one candidate; this is the one that does not, which is the case a devirtualising
compiler has to get right and a direct-calling one has to get wrong loudly

the mixed list is built by `setup`, so the two hundred allocations that fill it
are not counted against the sixty thousand dispatches that read it
"""


class Shape:
    def __init__(self, size: int):
        self.size = size

    def area(self) -> int:
        return self.size


class Square(Shape):
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
