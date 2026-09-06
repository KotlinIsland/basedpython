"""a running sum appended into a list the function owns

the growth case, and the only benchmark here that iterates a list with `for`
rather than by index — the two are different lowerings and `dot` covers the
other one

the list read from is built by `setup`, outside the clock. the list written to
is built by `prefix` itself on every call, which is the growth this row is for
"""


def prefix(xs: list[float]) -> float:
    out = []
    running = 0.0
    for x in xs:
        running = running + x
        out.append(running)
    return out[len(out) - 1]


_xs: list[float] = []


def setup() -> None:
    i = 0
    while i < 100000:
        _xs.append(i * 0.001)
        i = i + 1


def bench() -> float:
    total = 0.0
    r = 0
    while r < 5:
        total = total + prefix(_xs)
        r = r + 1
    return total
