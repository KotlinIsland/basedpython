"""a list built in a hot loop and handed to a generic function that reads it

the type parameter leaves the element type unknown where `consume` is written,
so the comparison in its body is python's `==` on whatever the lists hold, unless
the compiler specializes the call for the list it is given. `generic_mono` is the
same program with the call monomorphised by hand, so the comparison is between
two floats, and the gap between the two rows is what the type parameter costs

the body has to read the elements for that to be measured at all: a body that
only counts them never touches `T`, and both rows would compile to the same code
"""


def consume[T](xs: list[T], ys: list[T]) -> int:
    matched = 0
    i = 0
    while i < len(xs):
        if xs[i] == ys[i]:
            matched = matched + 1
        i = i + 1
    return matched


def bench() -> int:
    total = 0
    r = 0
    while r < 40:
        xs = [0.0]
        ys = [0.0]
        i = 0
        while i < 2000:
            xs.append(i * 0.5)
            ys.append((i - i % 2) * 0.5)
            i = i + 1
        total = total + consume(xs, ys)
        r = r + 1
    return total
