"""a scan over text, a character at a time: indexing and comparison

the text is made by repetition rather than by concatenation so that `words` —
which is the concatenation benchmark — cannot leak into this measurement

it is also made by `setup`, outside the clock. `str.__mul__` is one call into
cpython and runs at the same speed in every build, so leaving it inside
`bench()` put a fixed cost in the numerator and the denominator of a ratio whose
denominator this row is trying to make small — which caps the speedup the row
can report regardless of how fast the scan gets
"""


def text(n: int) -> str:
    unit = "word0 word1 word2 word3 word4 word5 word6 word7 word8 word9 "
    return unit * n


def longest_run(line: str) -> int:
    best = 0
    run = 0
    i = 0
    while i < len(line):
        if line[i] == " ":
            if run > best:
                best = run
            run = 0
        else:
            run = run + 1
        i = i + 1
    if run > best:
        best = run
    return best


_line: str = ""


def setup() -> None:
    global _line
    _line = text(2000)


def bench() -> int:
    total = 0
    r = 0
    while r < 10:
        total = total + longest_run(_line)
        r = r + 1
    return total
