"""a scan over text, a character at a time: indexing and comparison

the text is made by repetition rather than by concatenation so that `words` —
which is the concatenation benchmark — cannot leak into this measurement
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


def bench() -> int:
    line = text(2000)
    total = 0
    r = 0
    while r < 10:
        total = total + longest_run(line)
        r = r + 1
    return total
