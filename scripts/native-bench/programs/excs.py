"""a raise that is caught, on the path the program expects to take

every other benchmark stays on the straight line. this one leaves it: the
compiler emits an error edge from any fallible operation, and a `try` turns that
edge into a branch to a handler rather than to the function's own exit.

two shapes, because they cost differently. `caught` raises and catches across a
call boundary, which is the expensive one. `guarded` never raises at all and
measures what merely being inside a `try` costs a loop that succeeds — which
should be nothing, and is worth knowing rather than assuming. its callee is
`bounded`, which does almost no work, so that a cost the `try` added would be
most of the row rather than hidden under `parse`'s character scan
"""


class Refused(Exception):
    pass


def parse(text: str) -> int:
    if len(text) == 0:
        raise Refused("empty")
    total = 0
    i = 0
    while i < len(text):
        total = total + ord(text[i])
        i = i + 1
    return total


def bounded(value: int) -> int:
    if value < 0:
        raise Refused("negative")
    return value % 7


def caught(rounds: int) -> int:
    total = 0
    i = 0
    while i < rounds:
        try:
            total = total + parse("" if i % 4 == 0 else "abc")
        except Refused:
            total = total + 1
        i = i + 1
    return total


def guarded(rounds: int) -> int:
    """a loop inside a `try` whose handler is never reached"""
    total = 0
    i = 0
    while i < rounds:
        try:
            total = total + bounded(i)
        except Refused:
            total = total + 1
        i = i + 1
    return total


def bench() -> int:
    return caught(20000) + guarded(200000)
