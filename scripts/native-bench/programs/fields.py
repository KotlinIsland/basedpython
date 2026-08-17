"""attribute reads and writes on one object that outlives the loop

nothing is allocated inside the timed loop and no method is called, so this is
the field access on its own: on an emitted class that is an offset into a fixed
layout, and on an interpreted one it is a dict lookup through `__dict__`
"""


class State:
    def __init__(self):
        self.a = 0
        self.b = 1
        self.c = 2


def run(state: State, n: int) -> int:
    i = 0
    while i < n:
        state.a = state.b + state.c
        state.b = state.a - state.c
        state.c = state.c + 1
        i = i + 1
    return state.a + state.b + state.c


def bench() -> int:
    return run(State(), 300000)
