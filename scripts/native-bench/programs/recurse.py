"""calls that nest rather than repeat

`calls` measures a call from a loop, where the caller's frame is reused all the
way down. this one measures depth: every call is live while the next is made, so
it is about the stack the compiler builds rather than about the call sequence
"""


def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)


def bench() -> int:
    return fib(24)
