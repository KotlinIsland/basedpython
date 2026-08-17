"""integer arithmetic that leaves the machine word

a compiler that unboxes `int` has to decide what happens when the value stops
fitting. the answer is checked as well as timed: `bench()` returns a number far
wider than 64 bits, so a build that wrapped instead of promoting fails the
agreement check rather than posting a fast time
"""


def factorial(n: int) -> int:
    out = 1
    i = 2
    while i <= n:
        out = out * i
        i = i + 1
    return out


def digits(value: int) -> int:
    count = 0
    while value > 0:
        value = value // 10
        count = count + 1
    return count


def bench() -> int:
    total = 0
    r = 0
    while r < 40:
        total = total + digits(factorial(300))
        r = r + 1
    return total + factorial(60)
