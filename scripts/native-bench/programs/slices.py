"""slices of a list and of a string, which nothing else in the set takes

every container row here reads one element at a time: `dot` reads `a[i]`,
`chars` reads `line[i]`. a slice is a different operation — it allocates a new
object and copies a run of elements into it, and the strided form walks the
source with a step instead of copying a block. the three forms are here because
they are not one operation: `a[i:j]` is a bounded copy, `a[:n]` is the growing
one real code writes for a prefix, and `a[::2]` is the one that cannot be a
block copy at all

the list and the text are built by `setup`, outside every clock. built inside
`bench()` they would be the measurement, because building a list of five
hundred elements is `prefix`'s row and repeating a string is `chars`'s

both containers are short on purpose. a slice is linear in what it copies, so a
long one would make this a memory-bandwidth row and hide the per-slice cost —
the allocation, the bounds arithmetic and the dispatch through `__getitem__` —
which is the part a compiler can do anything about

each slice is consumed by `len`, which is the least this can do and still have
made the copy observable. anything more — indexing the result, summing it —
would be another row's operation added to this one
"""

_values: list[int] = []
_text: str = ""


def setup() -> None:
    global _text
    i = 0
    while i < 512:
        _values.append(i * 3)
        i = i + 1
    _text = "abcdefghij" * 52


def spans(values: list[int], line: str) -> int:
    total = 0
    i = 0
    while i < 256:
        total = total + len(values[i : i + 16])
        total = total + len(values[:i])
        total = total + len(values[::2])
        total = total + len(line[i : i + 16])
        total = total + len(line[:i])
        total = total + len(line[::2])
        i = i + 1
    return total


def bench() -> int:
    return spans(_values, _text)
