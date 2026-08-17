"""a sieve: a `bool` buffer the function owns, written from a nested loop

indexed *writes*, which `dot` and `prefix` between them do not cover — and a
list of `bool`, which is the element type a compiler is most tempted to pack and
most likely to get wrong
"""


def sieve(limit: int) -> int:
    flags = []
    i = 0
    while i < limit:
        flags.append(True)
        i = i + 1

    count = 0
    n = 2
    while n < limit:
        if flags[n]:
            count = count + 1
            m = n + n
            while m < limit:
                flags[m] = False
                m = m + n
        n = n + 1
    return count


def bench() -> int:
    return sieve(120000)
