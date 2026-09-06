"""the list `prefix` grows with `for` and `append`, built by a comprehension

read against `prefix`, which is this program's other half. both walk the same
hundred thousand floats, do one float add per element and grow one list to the
same length; the only thing between them is that `prefix` writes the loop and
the `append` out, and this hands the same job to a comprehension

that is not a formatting difference. a comprehension has a scope of its own, and
whether it is flattened into the enclosing function or left as a real call to a
nested one is what this pair asks. `prefix` carries its running sum in a local
where this adds a constant, because a comprehension has nowhere to keep an
accumulator — one float add either way

read the two rows' *speedups* rather than only their times, and read them
knowing that the interpreted halves differ too: cpython inlines a comprehension
and does not inline an `append` loop, so a compiler that lowers both to the same
code still reports a smaller number here than on `prefix`. the row that would
say something is wrong is the one where the two compiled times come apart

the list read from is built by `setup`, outside the clock. the list written to
is built on every call, which is the work this row is for
"""


def shifted(xs: list[float]) -> float:
    out = [x + 0.5 for x in xs]
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
        total = total + shifted(_xs)
        r = r + 1
    return total
