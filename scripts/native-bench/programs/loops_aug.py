"""the arithmetic of `loops`, written with augmented assignment

`n //= 2` and `running += …` ask an `int` subclass its in-place method before
its plain one, where `n = n // 2` does not, so the two spellings take different
slow paths. on exact ints they must cost the same: a gap between this row and
`loops` is the price of the in-place question leaking onto the fast path
"""


def collatz(n: int) -> int:
    steps = 0
    while n != 1:
        if n % 2 == 0:
            n //= 2
        else:
            n = 3 * n + 1
        steps += 1
    return steps


def total(limit: int) -> int:
    running = 0
    i = 1
    while i < limit:
        running += collatz(i)
        i += 1
    return running


def bench() -> int:
    return total(6000)
