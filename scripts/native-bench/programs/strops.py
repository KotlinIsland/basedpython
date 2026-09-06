"""the string methods real code calls: `split`, `join`, `startswith`, `upper`

`words` concatenates and `chars` indexes; between them they cover neither of the
two things python code actually does to a string, which is to take it apart and
put it back together through methods on `str`

the line those methods are called on is built by `setup`, outside the clock, for
the reason `chars` gives: the repetition that builds it is one call into cpython
and does not get faster in a compiled build
"""


def normalise(line: str) -> int:
    parts = line.split(" ")
    kept = []
    for part in parts:
        if part.startswith("w"):
            kept.append(part.upper())
    return len("-".join(kept))


def run(line: str, passes: int) -> int:
    total = 0
    p = 0
    while p < passes:
        total = total + normalise(line)
        p = p + 1
    return total


_line: str = ""


def setup() -> None:
    global _line
    unit = "word0 word1 word2 zero3 word4 word5 zero6 word7 word8 word9"
    _line = unit * 200


def bench() -> int:
    return run(_line, 60)
