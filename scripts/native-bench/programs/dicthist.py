"""the histogram shape: a membership test, then a read-modify-write on the hit

the word list is built once and counted many times, so counting is what the
number is about. the miss on the first sighting of each word is the branch that
makes this shape its own, and is why it is not folded into `dictget`

"once" means once per process, in `setup`. built inside `bench()` the twenty
thousand keys were a tenth of the operations this row did and every one of them
was a `"w" + str(...)`, which is `keybuild`'s measurement and costs several
times a dict operation
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


_words: list[str] = []


def setup() -> None:
    i = 0
    while i < 20000:
        _words.append("w" + str(i % 500))
        i = i + 1


def bench() -> int:
    return repeated(_words, 10)
