"""`generic`, with the call monomorphised by hand

identical in every other respect, on purpose: read the two rows as a pair and
the difference is what the type parameter costs. read either one alone and it
says almost nothing
"""


def consume(xs: list[float], ys: list[float]) -> int:
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
