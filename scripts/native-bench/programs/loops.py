"""tagged-integer arithmetic with no container, no call and no float

every other integer benchmark here carries something else: `sieve` owns a list,
`mandel` is floats, `recurse` is calls. this one is the arithmetic on its own,
so it is the floor a compiled loop can be measured against
"""


def collatz(n: int) -> int:
    steps = 0
    while n != 1:
        if n % 2 == 0:
            n = n // 2
        else:
            n = 3 * n + 1
        steps = steps + 1
    return steps


def total(limit: int) -> int:
    running = 0
    i = 1
    while i < limit:
        running = running + collatz(i)
        i = i + 1
    return running


def bench() -> int:
    return total(6000)
