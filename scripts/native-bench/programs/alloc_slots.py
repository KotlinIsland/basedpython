"""the same object as `alloc`, built and dropped, with `__slots__` declared

an emitted class carries a managed instance dict so that a name its body never
mentions can still be stored, the way its interpreted twin stores one. that dict
is four words on every instance, and for a two-field object it is most of what
allocation costs

`__slots__` is how python says an instance's attributes are exactly the declared
ones, and an emitted class that declares it gets the bare layout. so this program
is the escape hatch measured against `alloc`, which is the same source without
the declaration — the pair is the whole cost of the dict
"""


class Pair:
    __slots__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y


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
