"""tuples built, unpacked and indexed, and a function that returns two values

the container real code reaches for when it wants a pair, and the only way a
python function returns more than one thing. a tuple is immutable and of known
length, which is a different problem from `list` and is not covered by it
"""


def split(value: int) -> tuple[int, int]:
    return value // 7, value % 7


def run(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        whole, part = split(i)
        pair = (whole, part)
        total = total + pair[0] + pair[1]
        i = i + 1
    return total


def bench() -> int:
    return run(300000)
