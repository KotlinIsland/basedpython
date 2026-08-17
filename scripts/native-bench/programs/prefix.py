"""a running sum appended into a list the function owns

the growth case, and the only benchmark here that iterates a list with `for`
rather than by index — the two are different lowerings and `dot` covers the
other one
"""


def prefix(xs: list[float]) -> float:
    out = []
    running = 0.0
    for x in xs:
        running = running + x
        out.append(running)
    return out[len(out) - 1]


def bench() -> float:
    xs = []
    i = 0
    while i < 100000:
        xs.append(i * 0.001)
        i = i + 1
    total = 0.0
    r = 0
    while r < 5:
        total = total + prefix(xs)
        r = r + 1
    return total
