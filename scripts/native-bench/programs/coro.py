"""awaits that complete immediately, driven without an event loop

the loop is driven by hand rather than through `asyncio` on purpose: an event
loop would swamp the thing being measured with scheduling, and most awaits in
real code complete without ever suspending. what this times is the frame — its
creation, its resume, and the await of a coroutine that is already done
"""


async def step(i: int) -> int:
    return (i * 7) % 13


async def chain(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        total = total + await step(i)
        i = i + 1
    return total


def drive(n: int) -> int:
    """run a coroutine to completion, with no loop underneath it"""
    coroutine = chain(n)
    try:
        coroutine.send(None)
    except StopIteration as done:
        return done.value
    coroutine.close()
    return -1


def bench() -> int:
    total = 0
    run = 0
    while run < 20:
        total = total + drive(2000)
        run = run + 1
    return total
