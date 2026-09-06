"""a call whose arguments are not a plain positional list

`calls` is the floor: the same one-add body reached by two positional
arguments, so what this row adds to it is the argument shape and nothing else.
the callee takes a positional, a `*args` tuple the caller's extra argument has
to be packed into, a keyword-only parameter passed by name, a second
keyword-only one left to its default, and a `**kwargs` dict that has to be
built at the call and read back inside. every one of those is work done at the
boundary rather than in the body, which is the point of reading this against
`calls`

the body touches each of them once, so none of the packing can be argued away
as unobserved — a call that built a tuple and a dict nobody read would be a
call a compiler is free to stop making
"""


def tally(a: int, *rest: int, step: int = 1, scale: int = 1, **extra: int) -> int:
    return a + rest[0] + step + scale + extra["bonus"]


def run(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        total = tally(total, i, step=2, bonus=3)
        i = i + 1
    return total


def bench() -> int:
    return run(400000)
