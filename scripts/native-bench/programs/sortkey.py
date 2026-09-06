"""`sorted(..., key=...)`: a builtin calling back into a function of the module's

every call in this set is made by python code. this one is made by C: `sorted`
holds the loop, and the key function is reached from inside it once per element.
that is the direction nothing else here measures, and it is the direction a
compiled build cannot control — whatever the backend does to `weight`, the call
that reaches it comes from the interpreter's own sort. `min` and `max` with a
key are the same dispatch with the comparisons taken away

two things make this a stable measurement rather than a lucky one:

- **`sorted`, not `list.sort`.** sorting in place would leave the input sorted,
    so the second call would be given a different problem from the first and
    every round after the first would measure a nearly-ordered sort. `sorted`
    returns a new list and leaves its argument alone, so every round is handed
    the same permutation
- **the input is shuffled, and shuffled the same way every time.** timsort over
    an already-ordered list makes n-1 comparisons and finishes, which measures
    almost nothing; the order here comes from a fixed linear congruential
    sequence in `setup`, so it is disordered, and it is identical in all four
    builds — which the answer check then depends on

the key is deliberately many-to-one, so the sort has ties to break and the row
is not just a permutation lookup. timsort is stable, so the result is still
exactly determined by the input
"""

_values: list[int] = []


def setup() -> None:
    seed = 12345
    i = 0
    while i < 2000:
        seed = (seed * 1103515245 + 12345) % 2147483648
        _values.append(seed % 100000)
        i = i + 1


def weight(value: int) -> int:
    return value % 97


def bench() -> int:
    ordered = sorted(_values, key=weight)
    return ordered[0] + ordered[1000] + ordered[1999] + len(ordered)
