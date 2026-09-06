"""arithmetic and comparison reached through a class's own operator methods

the `dispatch` group has seven rows and every one of them reaches a method by
name. an operator does not: `a + b` goes to the type's number slot and `a < b`
to its comparison slot, and a class that defines `__add__`, `__lt__` and
`__eq__` is asking the compiler to fill those slots and then to call back
through them. that route is untested by everything else here

`methods` is the floor: the same body, called by name. what this row adds is the
slot, plus the allocation `__add__` makes because a value type has to answer
with a new one — `alloc` is the size of that half, and the three rows read
together say which of the two the operator costs
"""


class Money:
    amount: int

    def __init__(self, amount: int) -> None:
        self.amount = amount

    def __add__(self, other: "Money") -> "Money":
        return Money(self.amount + other.amount)

    def __lt__(self, other: "Money") -> bool:
        return self.amount < other.amount

    def __eq__(self, other: object) -> bool:
        return isinstance(other, Money) and self.amount == other.amount


def run(step: Money, cap: Money, n: int) -> int:
    held = 0
    running = Money(0)
    i = 0
    while i < n:
        running = running + step
        if running < cap:
            held = held + 1
        if running == cap:
            held = held + 2
        i = i + 1
    return held


def bench() -> int:
    return run(Money(1), Money(50000), 100000)
