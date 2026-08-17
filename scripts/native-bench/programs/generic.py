"""a list built in a hot loop and handed to a generic function

the type parameter is what stops the list being a buffer: the element
representation is not known at the definition, so neither the callee's body nor
the caller's list can be unboxed. `generic_mono` is the same program with the
call monomorphised by hand, and the gap between the two is the whole measurement
"""


def consume[T](xs: list[T], ys: list[T]) -> int:
    n = 0
    i = 0
    while i < len(xs):
        n = n + 1
        i = i + 1
    return n


def bench() -> int:
    total = 0
    r = 0
    while r < 40:
        xs = [0.0]
        ys = [0.0]
        i = 0
        while i < 2000:
            xs.append(i * 0.5)
            ys.append(i * 0.25)
            i = i + 1
        total = total + consume(xs, ys)
        r = r + 1
    return total
