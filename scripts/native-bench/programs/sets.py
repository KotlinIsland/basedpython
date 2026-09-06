"""a set built once, then tested for membership far more often than it grew

the hash container without a value, which `dict` does not stand in for: the
membership test is the whole operation rather than a step before a read. the
shape is a short build and a long test phase, so the answer is about `in`

the build is in `setup`, so "short" now means it costs this row nothing at all
rather than a few per cent of it
"""


def hits(seen: set[int], n: int, passes: int) -> int:
    found = 0
    p = 0
    while p < passes:
        i = 0
        while i < n:
            if i in seen:
                found = found + 1
            i = i + 1
        p = p + 1
    return found


_seen: set[int] = set()


def setup() -> None:
    i = 0
    while i < 2000:
        _seen.add(i * 3)
        i = i + 1


def bench() -> int:
    return hits(_seen, 2000, 30)
