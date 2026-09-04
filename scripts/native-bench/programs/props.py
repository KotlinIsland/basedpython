"""the same field access reached through a `@property` pair

the companion to `fields`: identical work, but every read goes through a getter
and every write through a setter, so the difference between the two rows is what
the descriptor costs. the pair is the commonest shape in the stdlib's own classes
and nothing else in this set has one — `logging`, `ssl`, `subprocess` and
`urllib.request` all publish lowered pairs
"""


class Cell:
    def __init__(self):
        self._v = 0

    @property
    def v(self) -> int:
        return self._v

    @v.setter
    def v(self, given: int):
        self._v = given


def run(cell: Cell, n: int) -> int:
    i = 0
    while i < n:
        cell.v = cell.v + 1
        i = i + 1
    return cell.v


def bench() -> int:
    return run(Cell(), 300000)
