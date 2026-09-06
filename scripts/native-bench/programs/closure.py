"""a nested function called in a loop, reading and writing a captured name

`calls` is the same shape with nothing captured, so what this row adds to it is
the capture: the enclosing local cannot live in either frame, because both the
maker and the made function reach it, so it lives in a cell and every read and
write of it is one indirection further away. the inner function also carries a
pointer to that cell for as long as it exists, which is a reference the plain
call never has to hold

the closure is made once and then called, so the row is about the calls rather
than about making it. and the inner function deliberately does not refer to its
own name: a closure that does holds a reference to itself through its own cell,
and that cycle is not something reference counting frees — which is a leak to
measure somewhere else, on purpose, not something to build into a row that runs
in every suite
"""

from collections.abc import Callable


def counter(start: int) -> Callable[[int], int]:
    total = start

    def step(by: int) -> int:
        nonlocal total
        total = total + by
        return total

    return step


def run(n: int) -> int:
    step = counter(0)
    last = 0
    i = 0
    while i < n:
        last = step(i)
        i = i + 1
    return last


def bench() -> int:
    return run(400000)
