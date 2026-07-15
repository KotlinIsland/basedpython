# basedpython: `Overlapping` — overlap-checked input on a covariant typevar

`ty_extensions.Overlapping[Key]` is a "safe variance" escape hatch for input positions. It lets a
covariant class take a `Key` argument without giving up covariance:

- at the call site, an argument is accepted iff it is *not disjoint from* `Key` (its type overlaps
    `Key`). so a provably-unrelated argument is rejected, but a could-be-a-`Key` argument is allowed
- inside the body, the parameter is seen as `Key`'s upper bound, so the consumed value can never be
    written back into `Key`-typed covariant storage

```toml
[environment]
python-version = "3.12"
```

## the `__contains__` use case

```py
from ty_extensions import Overlapping

class Mapping[Key, Value]:
    def __contains__(self, key: Overlapping[Key]) -> bool:
        reveal_type(key)  # revealed: object
        return True

def f(m: Mapping[int, object]):
    1 in m  # ok — `int` overlaps `int`
    object() in m  # ok — `object` overlaps `int` (it could be an `int`)
    # error: [unsupported-operator]
    "a" in m  # `str` is disjoint from `int`
```

## a direct method call is checked the same way

```py
from ty_extensions import Overlapping

class Mapping[Key, Value]:
    def __contains__(self, key: Overlapping[Key]) -> bool:
        return True

def f(m: Mapping[int, object]):
    m.__contains__(1)  # ok
    m.__contains__(object())  # ok
    # error: [invalid-argument-type]
    m.__contains__("a")
```

## the body is erased to the bound

```py
from ty_extensions import Overlapping

class Box[Key]:
    def consume(self, key: Overlapping[Key]) -> None:
        reveal_type(key)  # revealed: object
```

## a bounded typevar is erased to its bound

```py
from ty_extensions import Overlapping

class Box[Key: int]:
    def consume(self, key: Overlapping[Key]) -> None:
        reveal_type(key)  # revealed: int

def f(b: Box[int]):
    b.consume(1)  # ok — `int` overlaps `int`
    b.consume(True)  # ok — `bool` overlaps `int`
    # error: [invalid-argument-type]
    b.consume("a")
```

## overlap is exact: a sibling literal is disjoint

`Literal[1]` is not a `bool` (its only inhabitant is the int `1`), so it is disjoint from `bool` and
rejected for a `Box[bool]`:

```py
from ty_extensions import Overlapping

class Box[Key]:
    def consume(self, key: Overlapping[Key]) -> None: ...

def f(b: Box[bool]):
    b.consume(True)  # ok
    # error: [invalid-argument-type]
    b.consume(1)
```

## `Any` always overlaps

```py
from ty_extensions import Overlapping
from typing import Any

class Mapping[Key, Value]:
    def __contains__(self, key: Overlapping[Key]) -> bool:
        return True

def f(m: Mapping[int, object], a: Any):
    a in m  # ok — `Any` overlaps everything
```

## stdlib `dict` lookup uses it too

`dict.__getitem__` and `dict.get` consume the covariant key as `Overlapping[Key]`, so a lookup with
a provably-unrelated key is rejected, consistent with the membership check:

```py
def f(d: dict[int, str]):
    reveal_type(d[1])  # revealed: str
    d[object()]  # ok — object overlaps int
    # error: [invalid-argument-type]
    d["a"]
    d.get(1)  # ok
    # error: [invalid-argument-type]
    d.get("a")
```

A subclass may still override with the bare key type (or its upper bound) — the `Overlapping[Key]`
base relates as `Key`, so the override stays compatible:

```py
class MyDict[Key, Value](dict[Key, Value]):
    def __getitem__(self, key: Key) -> Value:  # ok — compatible override
        raise NotImplementedError
```

## the stdlib `Mapping.__contains__` uses it

basedpython's typeshed types `typing.Mapping.__contains__` as `Overlapping[Key]`, so a membership
test against a `Mapping[int, ...]` rejects a provably-unrelated key:

```py
from typing import Mapping

def f(m: Mapping[int, object]):
    1 in m  # ok
    object() in m  # ok
    # error: [unsupported-operator]
    "a" in m
```

## unions overlap if any member does

```py
from ty_extensions import Overlapping

class Mapping[Key, Value]:
    def __contains__(self, key: Overlapping[Key]) -> bool:
        return True

def f(m: Mapping[int | str, object]):
    1 in m  # ok
    "a" in m  # ok
    # error: [unsupported-operator]
    b"x" in m  # `bytes` is disjoint from `int | str`
```
