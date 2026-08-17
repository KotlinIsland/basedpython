"""the histogram shape: a membership test, then a read-modify-write on the hit

the word list is built once and counted many times, so counting is what the
number is about. the miss on the first sighting of each word is the branch that
makes this shape its own, and is why it is not folded into `dictget`
"""


def counted(words: list[str]) -> int:
    seen: dict[str, int] = {}
    for word in words:
        if word in seen:
            seen[word] = seen[word] + 1
        else:
            seen[word] = 1
    return len(seen)


def repeated(words: list[str], passes: int) -> int:
    total = 0
    p = 0
    while p < passes:
        total = total + counted(words)
        p = p + 1
    return total


def bench() -> int:
    words = []
    i = 0
    while i < 20000:
        words.append("w" + str(i % 500))
        i = i + 1
    return repeated(words, 10)
