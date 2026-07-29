# conditions

A condition collapses a value to a single bit. Two checks watch for the ways that goes wrong:
`overlapping-condition` when the selected branch holds a value that is always there alongside one
that is only sometimes there, and `redundant-condition` when the outcome is fixed and there is no
branch at all.

## a truthy test that tells `True` apart

Only the branch the condition *selects* is analysed. `bool | None` has exactly one truthy member, so
`if a` asks a question with one answer.

```py
def f(a: bool | None):
    if a:
        reveal_type(a)  # revealed: Literal[True]
```

## a falsy test that does not

The falsy side has two, and the branch cannot say which one it got.

```py
def f(a: bool | None):
    # error: [overlapping-condition] "This condition does not distinguish between `Literal[False]` and `None`"
    if not a:
        reveal_type(a)  # revealed: Literal[False] | None
```

## an optional string

The classic one: an empty string and an absent value share a branch.

```py
def f(name: str | None):
    # error: [overlapping-condition] "This condition does not distinguish between `str & ~AlwaysTruthy` and `None`"
    if not name: ...
```

## telling the members apart is fine

```py
def f(name: str | None):
    if name is None: ...
```

## a class that settles its own truthiness

An instance that always answers `True` is never a falsy member, so both directions are clean.

```py
from typing import Literal

class A:
    def __bool__(self) -> Literal[True]:
        return True

def f(a: A | None):
    if a:
        reveal_type(a)  # revealed: A
    if not a:
        reveal_type(a)  # revealed: None
```

## a class that does not

Without `__bool__`, a subclass may still be falsy, so `A` is a falsy member alongside `None`.

```py
class A: ...

def f(a: A | None):
    if a:
        reveal_type(a)  # revealed: A & ~AlwaysFalsy
    # error: [overlapping-condition] "This condition does not distinguish between `A & ~AlwaysTruthy` and `None`"
    if not a: ...
```

## `not not` is not a negation

Polarity is read off the whole chain of `not`s, so the selected branch is the truthy one again.

```py
def f(a: bool | None):
    if not not a:
        reveal_type(a)  # revealed: Literal[True]
```

## three overlapping members

```py
from typing import Literal

def f(a: Literal[0] | str | None):
    # error: [overlapping-condition] "This condition does not distinguish between `Literal[0]`, `str & ~AlwaysTruthy` and `None`"
    if not a: ...
```

## every condition position

Each position gets its own function: narrowing from one test would otherwise decide the next.

```py
def while_(a: bool | None) -> None:
    while not a:  # error: [overlapping-condition]
        ...

def assert_(a: bool | None) -> None:
    assert not a  # error: [overlapping-condition]

def if_expression(a: bool | None) -> None:
    x = 1 if not a else 2  # error: [overlapping-condition]

def comprehension(a: bool | None) -> None:
    y = [n for n in range(3) if not a]  # error: [overlapping-condition]

def match_guard(a: bool | None) -> None:
    match 1:
        case int() if not a:  # error: [overlapping-condition]
            ...
```

## two members that are each only partly in the branch

`if x:` puts a non-empty `str` next to a non-empty `bytes`, and neither of them was ever going to be
anywhere else — the union already conflated them and the condition added nothing. What the check
looks for is the asymmetry of one member being unconditionally in the branch.

```py
def f(x: str | bytes):
    if x: ...
    if not x: ...
```

## nor when every member is unconditionally there

```py
from typing import Literal

class Truthy:
    def __bool__(self) -> Literal[True]:
        return True

class AlsoTruthy:
    def __bool__(self) -> Literal[True]:
        return True

def f(x: Truthy | AlsoTruthy | None):
    if x: ...
```

## one class, two specializations, one kind

Type arguments are not what truthiness sees: `list[A]` and `list[B]` are both lists, and the branch
only learns that one of them is empty.

```py
class A: ...
class B: ...

def f(xs: list[A] | list[B]):
    if xs: ...
```

## a boolean operator is one condition per operand

`a and b` is not a single value being tested — each operand's truthiness is tested on its own — so
the operator's value (the union of the operands) is not analysed as if it were one member set.

```py
def f(count: int, leftovers: dict[str, int]):
    if count > 0 or leftovers: ...
```

## and each operand is checked on its own

```py
def f(a: bool | None, name: str | None):
    # error: [overlapping-condition] "This condition does not distinguish between `Literal[False]` and `None`"
    # error: [overlapping-condition] "This condition does not distinguish between `str & ~AlwaysTruthy` and `None`"
    if not a or not name: ...
```

## polarity carries into the operands

```py
from typing import Literal

def f(a: Literal[True], b: bool):
    # error: [redundant-condition] "This condition is always false"
    if not a and b: ...
```

## an attribute or a subscript is a value read too

```py
from typing import Literal

class Holder:
    flag: bool | None
    always: Literal[True]

def f(h: Holder, d: dict[str, bool | None]):
    # error: [overlapping-condition]
    if not h.flag: ...
    # error: [redundant-condition] "This condition is always true"
    if h.always: ...
    # error: [overlapping-condition]
    if not d["k"]: ...
```

## a protocol instance is an instance

```toml
[analysis]
overlapping-condition-assume-truthy-instances = true
```

```py
from typing import Protocol

class HasName(Protocol):
    name: str

def f(x: HasName | None):
    if not x: ...
```

## exempting a type

`analysis.overlapping-condition-exempt-types` says a member is not worth telling apart. With `int`
exempt, only `None` is left in the falsy branch.

```toml
[analysis]
overlapping-condition-exempt-types = ["int"]
```

```py
def f(a: int | None):
    if not a: ...

def g(a: str | None):
    # error: [overlapping-condition]
    if not a: ...
```

## exempting `None`

```toml
[analysis]
overlapping-condition-exempt-types = ["None"]
```

```py
def f(a: int | None):
    if not a: ...
```

## exempting a qualified name

```toml
[analysis]
overlapping-condition-exempt-types = ["decimal.Decimal"]
```

```py
from decimal import Decimal

def f(a: Decimal | None):
    if not a: ...
```

## assuming an instance is truthy

`analysis.overlapping-condition-assume-truthy-instances` takes a class with no `__bool__` and no
`__len__` at face value, which drops the report for `if not x` over an optional instance.

```toml
[analysis]
overlapping-condition-assume-truthy-instances = true
```

```py
class A: ...

def f(a: A | None):
    if not a:
        reveal_type(a)  # revealed: (A & ~AlwaysTruthy) | None
```

## a class that does define `__len__` is unaffected

```toml
[analysis]
overlapping-condition-assume-truthy-instances = true
```

```py
class A:
    def __len__(self) -> int:
        return 0

def f(a: A | None):
    # error: [overlapping-condition]
    if not a: ...
```

## a condition that is always true

```py
from typing import Literal

def f(a: Literal[True]):
    # error: [redundant-condition] "This condition is always true"
    if a: ...
```

## a condition that is always false

```py
from typing import Literal

def f(a: Literal[""]):
    # error: [redundant-condition] "This condition is always false"
    if a: ...
```

## a literal condition is deliberate

`while True` and `assert False` mean the constant; they are not a conditional that failed to be
conditional.

```py
def f():
    while True:
        break
    if False: ...
    assert False
```

## `TYPE_CHECKING` is artificially constant

Its constant outcome is the checker's doing, not the program's.

```py
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import decimal

if not TYPE_CHECKING: ...
```

## `sys.version_info` is artificially constant

```toml
[environment]
python-version = "3.12"
```

```py
import sys

if sys.version_info >= (3, 10): ...

if sys.version_info < (3, 10): ...
```

## `sys.platform` is artificially constant

```toml
[environment]
python-platform = "linux"
```

```py
import sys

if sys.platform == "win32": ...

if sys.platform.startswith("linux"): ...
```

## `os.name` is artificially constant

```toml
[environment]
python-platform = "linux"
```

```py
import os

if os.name == "nt": ...
```

## an aliased `sys` still works

```toml
[environment]
python-version = "3.12"
```

```py
import sys as system

if system.version_info >= (3, 10): ...
```

## comparing a `bool` with `True`

```py
def f(a: bool):
    # error: [redundant-boolean-comparison] "Comparison of a `bool` with `True` is redundant"
    if a == True: ...
```

## comparing a `bool` with `False`

```py
def f(a: bool):
    # error: [redundant-boolean-comparison] "Comparison of a `bool` with `False` is redundant"
    if a is False: ...
```

## the literal may come first

```py
def f(a: bool):
    # error: [redundant-boolean-comparison]
    if True != a: ...
```

## an optional `bool` is not redundant

`a == True` really does tell `True` apart from `False` and `None` here.

```py
def f(a: bool | None):
    if a == True: ...
```

## an `int` is not redundant

```py
def f(a: int):
    if a == True: ...
```

## a chained comparison is left alone

Two comparisons over one literal: reporting each pair would double up on that `True`, and neither
the operand nor its negation replaces the chain.

```py
def f(a: bool, b: bool):
    if a == True == b: ...
```

## the comparison need not be a condition

```py
def f(a: bool) -> bool:
    # error: [redundant-boolean-comparison] "Comparison of a `bool` with `True` is redundant"
    y = a == True
    return y
```

## a negated operator flips the advice

```py
def f(a: bool):
    # error: [redundant-boolean-comparison] "Comparison of a `bool` with `False` is redundant"
    if a is not False: ...
    # error: [redundant-boolean-comparison] "Comparison of a `bool` with `False` is redundant"
    if a != False: ...
```

## an operand bounded by `bool`

```toml
[environment]
python-version = "3.12"
```

```py
def f[T: bool](t: T):
    # error: [redundant-boolean-comparison]
    if t == True: ...
```

## an ordering comparison is not redundant

```py
def f(a: bool):
    if a < True: ...
```

## diagnostics

```py
from typing import Literal

def overlap(a: bool | None):
    # snapshot: overlapping-condition
    if not a: ...

def redundant(a: Literal[True]):
    # snapshot: redundant-condition
    if a: ...

def comparison(a: bool):
    # snapshot: redundant-boolean-comparison
    if a == False: ...
```

```snapshot
warning[overlapping-condition]: This condition does not distinguish between `Literal[False]` and `None`
 --> src/mdtest_snippet.py:5:8
  |
5 |     if not a: ...
  |        ^^^^^
  |
info: `bool | None` is tested for falsiness
help: Compare against the specific value instead of testing truthiness


warning[redundant-condition]: This condition is always true
 --> src/mdtest_snippet.py:9:8
  |
9 |     if a: ...
  |        ^
  |
info: `Literal[True]` is always truthy


warning[redundant-boolean-comparison]: Comparison of a `bool` with `False` is redundant
  --> src/mdtest_snippet.py:13:8
   |
13 |     if a == False: ...
   |        ^^^^^^^^^^
   |
info: `bool` already is the value this comparison produces
help: Negate the operand with `not` instead
```
