"""a pair whose first slot holds a reference rather than a machine word

`tuples` is all-`int`, so every slot it has fits in a tagged word and nothing in
it is ever retained or released. a slot holding an object is the other half of
the representation, and the retain and release around it are what that half
costs. the object is built once outside the loop, so what this adds to `tuples`
is the slot and not an allocation
"""


class Cell:
    x: int

    def __init__(self, x: int) -> None:
        self.x = x


def split(cell: Cell, value: int) -> tuple[Cell, int]:
    return cell, value % 7


def run(n: int) -> int:
    cell = Cell(3)
    total = 0
    i = 0
    while i < n:
        held, part = split(cell, i)
        total = total + held.x + part
        i = i + 1
    return total


def bench() -> int:
    return run(300000)
