"""a generator consumed by a `for` loop

the compiler turns a resumable frame into a state object: locals parked in
fields, a tag saying whether a suspension was a `yield` or an `await`, and a
resume method. `coro` covers the await half of that; this is the yield half, and
it is also the half real code writes far more often
"""


def steps(n: int):
    i = 0
    while i < n:
        yield (i * 7) % 13
        i = i + 1


def consume(n: int) -> int:
    total = 0
    for value in steps(n):
        total = total + value
    return total


def bench() -> int:
    total = 0
    r = 0
    while r < 20:
        total = total + consume(2000)
        r = r + 1
    return total
