# conditions

A condition collapses a value to a single bit. Three checks watch for the ways that goes wrong:
`truthiness-test-of-none-union` when an optional value's `None` shares a branch with another falsy
value, `overlapping-condition` when the selected branch holds any other value that is always there
alongside one that is only sometimes there, and `redundant-condition` when the outcome is fixed and
there is no branch at all.

## an optional string

The classic one: an empty string and an absent value share a branch.

```py
def f(name: str | None):
    # error: [truthiness-test-of-none-union] "Boolean test on `str | None` does not distinguish `None` from other falsy values: `None` and the empty string are both falsy"
    if not name: ...
```

## telling the members apart is fine

```py
def f(name: str | None):
    if name is None: ...
```

## an optional instance is taken to be truthy

A class with no `__bool__` and no `__len__` is taken at face value, so `None` is the only falsy
member.

```py
class A: ...

def f(a: A | None):
    if not a: ...
```

## an optional instance that defines `__len__` is not

```py
class A:
    def __len__(self) -> int:
        return 0

def f(a: A | None):
    # error: [truthiness-test-of-none-union]
    if not a: ...
```

## a truthy test that tells the members apart

Only the branch the condition *selects* is analysed. `list[int] | Literal[False]` has one kind of
truthy member, so `if a` asks a question with one answer.

```py
from typing import Literal

def f(a: list[int] | Literal[False]):
    if a:
        reveal_type(a)  # revealed: list[int] & ~AlwaysFalsy
```

## a falsy test that does not

The falsy side has two, and the branch cannot say which one it got.

```py
from typing import Literal

def f(a: list[int] | Literal[False]):
    # error: [overlapping-condition] "This condition does not distinguish between `list[int] & ~AlwaysTruthy` and `Literal[False]`"
    if not a:
        reveal_type(a)  # revealed: (list[int] & ~AlwaysTruthy) | Literal[False]
```

## a class that settles its own truthiness

An instance that always answers `True` is never a falsy member, so both directions are clean.

```py
from typing import Literal

class A:
    def __bool__(self) -> Literal[True]:
        return True

def f(a: A | Literal[False]):
    if a:
        reveal_type(a)  # revealed: A
    if not a:
        reveal_type(a)  # revealed: Literal[False]
```

## a class that does not

Without `__bool__`, a subclass may still be falsy, so `A` is a falsy member alongside `False`.

```py
from typing import Literal

class A: ...

def f(a: A | Literal[False]):
    if a:
        reveal_type(a)  # revealed: A & ~AlwaysFalsy
    # error: [overlapping-condition] "This condition does not distinguish between `A & ~AlwaysTruthy` and `Literal[False]`"
    if not a: ...
```

## `not not` is not a negation

Polarity is read off the whole chain of `not`s, so the selected branch is the truthy one again.

```py
from typing import Literal

def f(a: list[int] | Literal[False]):
    if not not a:
        reveal_type(a)  # revealed: list[int] & ~AlwaysFalsy
```

## three overlapping members

```py
from typing import Literal

def f(a: Literal[0] | str | bytes):
    # error: [overlapping-condition] "This condition does not distinguish between `Literal[0]`, `str & ~AlwaysTruthy` and `bytes & ~AlwaysTruthy`"
    if not a: ...
```

## every condition position

Each position gets its own function: narrowing from one test would otherwise decide the next.

```py
from typing import Literal

def while_(a: str | Literal[False]) -> None:
    while not a:  # error: [overlapping-condition]
        ...

def assert_(a: str | Literal[False]) -> None:
    assert not a  # error: [overlapping-condition]

def if_expression(a: str | Literal[False]) -> None:
    x = 1 if not a else 2  # error: [overlapping-condition]

def comprehension(a: str | Literal[False]) -> None:
    y = [n for n in range(3) if not a]  # error: [overlapping-condition]

def match_guard(a: str | Literal[False]) -> None:
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

def f(x: Truthy | AlsoTruthy | Literal[False]):
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
from typing import Literal

def f(a: list[int] | Literal[False], name: str | Literal[False]):
    # error: [overlapping-condition] "This condition does not distinguish between `list[int] & ~AlwaysTruthy` and `Literal[False]`"
    # error: [overlapping-condition] "This condition does not distinguish between `str & ~AlwaysTruthy` and `Literal[False]`"
    if not a or not name: ...
```

## an attribute or a subscript is a value read too

```py
from typing import Literal

class Holder:
    flag: str | Literal[False]

def f(h: Holder, d: dict[str, str | Literal[False]]):
    # error: [overlapping-condition]
    if not h.flag: ...
    # error: [overlapping-condition]
    if not d["k"]: ...
```

## an attribute narrowed by an earlier test is not decided

A call between the narrowing and the read may have written to the attribute, and holding the
narrowed type across it is what makes attribute narrowing usable. What the class declares is the
fact, so that is what decides the condition.

```py
class Latch:
    on: bool

    def flip(self) -> None:
        self.on = True

def f(latch: Latch) -> None:
    latch.on = False
    latch.flip()
    if latch.on: ...
```

## an attribute a property reads is not decided either

The property reads the attribute the call wrote, so the same reasoning reaches through it.

```py
class Machine:
    running: bool

    @property
    def alive(self) -> bool:
        return self.running

    def tick(self) -> None:
        self.running = False

def f(machine: Machine) -> None:
    while machine.alive:
        machine.tick()
        if not machine.alive: ...
```

## a protocol instance is an instance

```toml
[analysis]
overlapping-condition-assume-truthy-instances = true
```

```py
from typing import Literal, Protocol

class HasName(Protocol):
    name: str

def f(x: HasName | Literal[False]):
    if not x: ...
```

## exempting a type

`analysis.overlapping-condition-exempt-types` says a member is not worth telling apart. With `str`
exempt, only `False` is left in the falsy branch.

```toml
[analysis]
overlapping-condition-exempt-types = ["str"]
```

```py
from typing import Literal

def f(a: str | Literal[False]):
    if not a: ...

def g(a: bytes | Literal[False]):
    # error: [overlapping-condition]
    if not a: ...
```

## exempting a qualified name

```toml
[analysis]
overlapping-condition-exempt-types = ["decimal.Decimal"]
```

```py
from decimal import Decimal
from typing import Literal

def f(a: Decimal | Literal[False]):
    if not a: ...
```

## assuming an instance is truthy

`analysis.overlapping-condition-assume-truthy-instances` takes a class with no `__bool__` and no
`__len__` at face value, which drops the report for `if not x` over an instance or a `False`
sentinel.

```toml
[analysis]
overlapping-condition-assume-truthy-instances = true
```

```py
from typing import Literal

class A: ...

def f(a: A | Literal[False]):
    if not a:
        reveal_type(a)  # revealed: (A & ~AlwaysTruthy) | Literal[False]
```

## a class that does define `__len__` is unaffected

```toml
[analysis]
overlapping-condition-assume-truthy-instances = true
```

```py
from typing import Literal

class A:
    def __len__(self) -> int:
        return 0

def f(a: A | Literal[False]):
    # error: [overlapping-condition]
    if not a: ...
```

## a condition that is always true

```py
def f(a: tuple[int]):
    # error: [redundant-condition]
    if a: ...
```

## a condition that is always false

```py
from typing import Literal

def f(a: Literal[""]):
    # error: [redundant-condition]
    if a: ...
```

## a bare `case` name that captures is not an environment guard

a value decided by an environment guard such as `sys.platform` is exempt from `redundant-condition`,
since the guard can go the other way on another platform. a bare `case q:` over such a subject only
captures it when `q` is not a member of the subject's enum, and a capture matches every value, so it
guards nothing and the code after the `match` is not exempt

```by
import sys

match sys.platform:
    case q:
        pass

y = "fixed"
# error: [redundant-condition]
if y:
    pass
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

def overlap(a: list[int] | Literal[False]):
    # snapshot: overlapping-condition
    if not a: ...

def redundant(a: tuple[int]):
    # snapshot: redundant-condition
    if a: ...

def comparison(a: bool):
    # snapshot: redundant-boolean-comparison
    if a == False: ...
```

```snapshot
warning[overlapping-condition]: This condition does not distinguish between `list[int] & ~AlwaysTruthy` and `Literal[False]`
 --> src/mdtest_snippet.py:5:8
  |
5 |     if not a: ...
  |        ^^^^^
info: `list[int] | Literal[False]` is tested for falsiness
help: Compare against the specific value instead of testing truthiness


warning[redundant-condition]: A 1-element tuple is always truthy
 --> src/mdtest_snippet.py:9:8
  |
7 | def redundant(a: tuple[int]):
  |                  ----------
  |                  |
  |                  Inferred as a 1-element tuple due to this annotation
  |                  Did you mean `tuple[int, ...]`?
8 |     # snapshot: redundant-condition
9 |     if a: ...
  |        ^ Inferred type `tuple[int]` is always truthy


warning[redundant-boolean-comparison]: Comparison of a `bool` with `False` is redundant
  --> src/mdtest_snippet.py:13:8
   |
13 |     if a == False: ...
   |        ^^^^^^^^^^
info: `bool` already is the value this comparison produces
help: Negate the operand with `not` instead
```
