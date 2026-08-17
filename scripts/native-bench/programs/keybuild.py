"""`"k" + str(i)`: a builtin through the module namespace, a call, and a concat

there is no dict here at all. this expression sat in every loop of the dict
benchmarks and was most of what they measured, so it is timed on its own and
they hand their keys in already built. `len` is called once, at the end
"""


def keys(n: int, passes: int) -> int:
    last = "k"
    p = 0
    while p < passes:
        i = 0
        while i < n:
            last = "k" + str(i)
            i = i + 1
        p = p + 1
    return len(last)


def bench() -> int:
    return keys(2000, 25)
