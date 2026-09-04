"""a `@property` getter with no setter written under it

python folds a group of one into a `property` exactly as it folds a pair, so the
read goes the same way round the descriptor protocol either way — the source
difference between this and `props` is the two lines `props` writes under
`@v.setter`, and the loop here reads what that loop reads. `fields` is the same
read reached directly, so the three rows say what each layer costs
"""


class Cell:
    def __init__(self):
        self._v = 1

    @property
    def v(self) -> int:
        return self._v


def run(cell: Cell, n: int) -> int:
    total = 0
    i = 0
    while i < n:
        total = total + cell.v
        i = i + 1
    return total


def bench() -> int:
    return run(Cell(), 300000)
