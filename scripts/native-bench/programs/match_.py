"""a `match` over subjects of several shapes, dispatched by pattern

a `match` is not sugar for a chain of `if`. each case is a test the compiler has
to build for itself — an equality against a literal, a type test followed by
positional reads through `__match_args__`, a guard evaluated only after its
pattern bound, and a wildcard that always takes. this row is the whole ladder,
walked once per subject

the subject is `object`, which is the shape that makes the ladder real: every
class pattern is a runtime type test rather than something a declared type
already settled. the arms are ordered so that no single one takes most of the
traffic, and the guard arm both succeeds and fails, so the row is not a
measurement of one lucky case

`excs` is the neighbouring row on branching, and it measures a handler that a
raise reaches. nothing about a `match` raises; what this measures is the
dispatch
"""


class Point:
    x: int
    y: int

    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int) -> None:
        self.x = x
        self.y = y


def classify(value: object) -> int:
    match value:
        case 0:
            return 1
        case Point(a, b) if a > b:
            return a - b
        case Point(a, b):
            return a + b
        case str() as text:
            return len(text)
        case _:
            return 3


def sweep(cases: list[object], passes: int) -> int:
    total = 0
    p = 0
    while p < passes:
        i = 0
        while i < len(cases):
            total = total + classify(cases[i])
            i = i + 1
        p = p + 1
    return total


_cases: list[object] = []


def setup() -> None:
    i = 0
    while i < 400:
        _cases.append(0)
        _cases.append(Point(i, 1))
        _cases.append(Point(1, i))
        _cases.append("abc")
        _cases.append(i + 1)
        i = i + 1


def bench() -> int:
    return sweep(_cases, 15)
