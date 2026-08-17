"""a set built once, then tested for membership far more often than it grew

the hash container without a value, which `dict` does not stand in for: the
membership test is the whole operation rather than a step before a read. the
shape is a short build and a long test phase, so the answer is about `in`
"""


def build(n: int) -> set[int]:
    seen: set[int] = set()
    i = 0
    while i < n:
        seen.add(i * 3)
        i = i + 1
    return seen


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


def bench() -> int:
    seen = build(2000)
    return hits(seen, 2000, 30)
