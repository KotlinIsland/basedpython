"""the `props` pair on a class another class in the module extends

`props` measures a property pair on a class nothing extends, which is the shape
the direct call to a half is licensed for. a class with an in-module subclass is
emitted as a mutable heap type instead, because a subclass may override a half,
and every read and write of the pair goes the whole way round the descriptor
protocol again. the two rows together are what that costs — `Sized` is here only
to be that subclass, and the work `run` does is `props`'s exactly
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


class Sized(Cell):
    def width(self) -> int:
        return 1


def run(cell: Cell, n: int) -> int:
    i = 0
    while i < n:
        cell.v = cell.v + 1
        i = i + 1
    return cell.v


def bench() -> int:
    return run(Cell(), 300000) + Sized().width()
