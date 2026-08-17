"""string concatenation in a loop, and nothing else

building and scanning are separate problems with separate answers, so `chars`
scans a string of the same shape and this one only builds. measuring them
together hides whichever is cheaper
"""


def build(n: int) -> str:
    out = ""
    i = 0
    while i < n:
        out = out + "word" + str(i % 10) + " "
        i = i + 1
    return out


def bench() -> int:
    return len(build(20000))
