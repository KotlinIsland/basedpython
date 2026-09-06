"""a `with` block over a class of this module's own, entered and left every pass

nothing else here leaves the straight line the way this does. `excs` covers the
error edge, but a `with` is the other half of the same machinery and a shape
real code writes constantly: the block's normal exit and its unwinding exit both
have to call `__exit__`, so the body sits inside an implicit `try` whose handler
is generated rather than written

the pair of methods is defined here rather than reached through `contextlib`,
because a `contextlib.contextmanager` is a generator wrapped by a class from a
module this build does not compile — that would time the stdlib. `Guard` is the
protocol spelled out, which is what the lowering has to recognise

`methods` is the floor under this row: the same two calls on the same
long-lived object, made by name instead of by the block
"""


class Guard:
    depth: int

    def __init__(self) -> None:
        self.depth = 0

    def __enter__(self) -> "Guard":
        self.depth = self.depth + 1
        return self

    def __exit__(self, kind: object, value: object, trace: object) -> None:
        self.depth = self.depth - 1


def run(guard: Guard, n: int) -> int:
    total = 0
    i = 0
    while i < n:
        with guard:
            total = total + guard.depth
        i = i + 1
    return total


def bench() -> int:
    return run(Guard(), 200000)
