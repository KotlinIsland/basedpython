"""awaits of a coroutine that finishes without handing back a value

`coro` times an `await` whose coroutine returns an int, which the compiled
frame reports through the send slot as a value it already holds. the far more
common shape in real code is an `async def` that returns nothing at all, and
that one used to end by *raising* `StopIteration` for its awaiter to unpack —
so the pair isolates what a finish costs when there is no value to carry
"""


async def step(i: int):
    return


async def chain(n: int) -> int:
    i = 0
    while i < n:
        await step(i)
        i = i + 1
    return i


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
