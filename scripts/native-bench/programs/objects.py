"""a short-lived object that is then used: allocation, fields and methods together

deliberately the mixture, because that is what object-shaped code looks like.
`alloc`, `fields` and `methods` are its three parts measured apart, and a change
that moves this one without moving any of those is worth explaining
"""


class Vec:
    def __init__(self, x: float, y: float):
        self.x = x
        self.y = y

    def norm2(self) -> float:
        return self.x * self.x + self.y * self.y

    def shift(self, dx: float, dy: float) -> float:
        self.x = self.x + dx
        self.y = self.y + dy
        return self.x + self.y


def bench() -> float:
    total = 0.0
    i = 0
    while i < 200000:
        v = Vec(i * 0.5, i * 0.25)
        total = total + v.norm2() + v.shift(1.0, 2.0)
        i = i + 1
    return total
