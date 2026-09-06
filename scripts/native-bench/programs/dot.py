"""indexed reads over two lists that never escape the function

the list is the one container the compiler can hold as a buffer, so this is the
best case for it: no growth, no writes, and the element type known at the
definition

the two lists are built by `setup`, which the harness calls once per process
before it starts a clock. built inside `bench()` they were about a tenth of the
loop iterations this row ran, and every one of those did two appends — which is
`prefix`'s measurement rather than this one's
"""


def dot(a: list[float], b: list[float]) -> float:
    out = 0.0
    i = 0
    while i < len(a):
        out = out + a[i] * b[i]
        i = i + 1
    return out


_a: list[float] = []
_b: list[float] = []


def setup() -> None:
    i = 0
    while i < 50000:
        _a.append(i * 0.5)
        _b.append(i * 0.25)
        i = i + 1


def bench() -> float:
    total = 0.0
    r = 0
    while r < 10:
        total = total + dot(_a, _b)
        r = r + 1
    return total
