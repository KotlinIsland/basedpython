"""an object built and dropped every iteration, and nothing kept

allocation, the field stores the constructor makes, and the deallocation that
follows when the last reference goes. `objects` allocates too but then calls
methods on what it built, so its number is a mixture; this one is the mixture's
allocation half on its own
"""


class Pair:
    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y


def run(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        pair = Pair(i, i + 1)
        total = total + pair.x
        i = i + 1
    return total


def bench() -> int:
    return run(300000)
