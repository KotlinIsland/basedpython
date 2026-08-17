"""a plain function call, with as little else in the loop as it can have

the body is one add, so what the loop measures is the calling convention: the
frame, the argument passing, and the error check after the return. `mandel` has
a call on its hot path too but pays for float arithmetic around it, which is the
thing that hid a call regression once already
"""


def add(a: int, b: int) -> int:
    return a + b


def run(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        total = add(total, i)
        i = i + 1
    return total


def bench() -> int:
    return run(400000)
