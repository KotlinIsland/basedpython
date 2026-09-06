"""module-level names read from inside a loop, rather than passed in

every other benchmark here hands its loop everything it needs as a parameter or
a local, which is the shape a compiler finds easiest and the shape real modules
least often have. a module global is a different thing entirely: interpreted, it
is a dict lookup per read, and compiled it is whatever representation the module
decided on — which is exactly where a global read has been got wrong before, by
narrowing one to a representation its writer never agreed to

so both reads in the loop are globals: `_limit` in the condition and `_step` in
the body. `loops` is the floor under this row — the same arithmetic with both of
them held in locals — and the difference between the two is the cost of the name

`setup` is what writes them, so the values cannot be folded in from the
assignments above and, with the setup withheld, the loop has a bound of zero
"""

_step = 0
_limit = 0


def total() -> int:
    acc = 0
    i = 0
    while i < _limit:
        acc = acc + _step
        i = i + 1
    return acc


def setup() -> None:
    global _step, _limit
    seen = 0
    i = 0
    while i < 6:
        seen = seen + i
        i = i + 1
    _step = seen
    _limit = 200000


def bench() -> int:
    return total()
