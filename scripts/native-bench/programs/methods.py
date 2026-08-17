"""a method call on a long-lived object, with the object built once

the companion to `calls`: same trivial body, reached through an instance rather
than through a module global. the difference between the two is what the method
lookup costs, and neither of them allocates — `alloc` is where allocation is
measured
"""


class Counter:
    def __init__(self, base: int) -> None:
        self.base = base

    def step(self, k: int) -> int:
        return self.base + k


def run(counter: Counter, n: int) -> int:
    total = 0
    i = 0
    while i < n:
        total = total + counter.step(i)
        i = i + 1
    return total


def bench() -> int:
    return run(Counter(3), 300000)
