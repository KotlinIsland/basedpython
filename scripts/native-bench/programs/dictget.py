"""dict lookup and in-place update, with the keys handed in already built

the keys are built by `setup` and the timed loop only subscripts, so this is the
lookup question and nothing else. `str` keys, because that is what a real table
is keyed by and where cpython's own lookup is most specialised

the table outlives the call that updates it, so the counts it holds go on rising
for as long as the process times this benchmark. that changes what is stored and
not what it costs: the largest count any run of this harness can reach is a few
billion, which is still one machine word
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


_keys: list[str] = []
_table: dict[str, int] = {}


def setup() -> None:
    i = 0
    while i < 2000:
        key = "k" + str(i)
        _keys.append(key)
        _table[key] = i
        i = i + 1


def bench() -> int:
    return total(_table, _keys, 50)
