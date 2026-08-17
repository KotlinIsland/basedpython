"""indexed reads over two lists that never escape the function

the list is the one container the compiler can hold as a buffer, so this is the
best case for it: no growth, no writes, and the element type known at the
definition
"""


def dot(a: list[float], b: list[float]) -> float:
    out = 0.0
    i = 0
    while i < len(a):
        out = out + a[i] * b[i]
        i = i + 1
    return out


def bench() -> float:
    a = []
    b = []
    i = 0
    while i < 50000:
        a.append(i * 0.5)
        b.append(i * 0.25)
        i = i + 1
    total = 0.0
    r = 0
    while r < 10:
        total = total + dot(a, b)
        r = r + 1
    return total
