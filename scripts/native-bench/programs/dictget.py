"""dict lookup and in-place update, with the keys handed in already built

the keys are built once and the timed loop only subscripts, so this is the
lookup question and nothing else. `str` keys, because that is what a real table
is keyed by and where cpython's own lookup is most specialised
"""


def total(table: dict[str, int], keys: list[str], passes: int) -> int:
    running = 0
    p = 0
    n = len(keys)
    while p < passes:
        i = 0
        while i < n:
            key = keys[i]
            running = running + table[key]
            table[key] = table[key] + 1
            i = i + 1
        p = p + 1
    return running


def bench() -> int:
    keys = []
    table: dict[str, int] = {}
    i = 0
    while i < 2000:
        key = "k" + str(i)
        keys.append(key)
        table[key] = i
        i = i + 1
    return total(table, keys, 50)
