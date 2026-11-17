# Checked operator operands

python calls an operator dunder with whatever the other operand is, and an operand outside the
domain of the parameter's annotation is read as handed back with `NotImplemented` (see
`basedpython_operator_domains.md`). a `.by` dunder is lowered with the test that hands it back. a
`.py` one has to be written with it, and `checked-operator-operands` checks that it is

```toml
[analysis]
checked-operator-operands = true
```

## The body sees every operand python passes

until the body tests it, the operand is the annotation or anything outside its domain

```py
class Money:
    def __add__(self, other: "Money") -> "Money":
        reveal_type(other)  # revealed: object
        if not isinstance(other, Money):
            reveal_type(other)  # revealed: ~Money
            return NotImplemented
        reveal_type(other)  # revealed: Money
        return Money()

reveal_type(Money() + Money())  # revealed: Money
```

## A generic annotation is tested by its class

an operand inside the domain matched the whole annotation at the call that dispatched to the method,
so testing the class is enough to get the annotation back

```py
class Total:
    def __add__(self, other: list[int]) -> int:
        reveal_type(other)  # revealed: list[int] | ~Top[list[Unknown]]
        if isinstance(other, list):
            reveal_type(other)  # revealed: list[int]
            return sum(other)
        return NotImplemented
```

## A body that returns a value for an operand it never tested

dispatch has already sent an operand outside the domain to the other operand, so a value returned
for one answers an operation typed as answered somewhere else

```py
class Bad:
    def __add__(self, other: int) -> int:
        return 1  # snapshot: unguarded-operand
```

```snapshot
error[unguarded-operand]: `other` may be outside `int` where this returns
 --> src/mdtest_snippet.py:3:9
  |
3 |         return 1  # snapshot: unguarded-operand
  |         ^^^^^^^^
info: python hands `__add__` any operand, and the operation is typed as moving on from one outside `int`
help: return `NotImplemented` for an operand outside `int` before returning or raising anything else
```

## Falling off the end of the body returns a value

```py
class Log:
    def __iadd__(self, other: str) -> None:  # error: [unguarded-operand] "`other` may be outside `str` where the body ends"
        print(other)
```

## Only a return that hands back a value has to be tested

```py
class Point:
    x: int = 0

    def __eq__(self, other: "Point") -> bool:
        if isinstance(other, Point):
            return self.x == other.x
        return NotImplemented  # no diagnostic

class Loose:
    x: int = 0

    def __eq__(self, other: "Loose") -> bool:
        # error: [unresolved-attribute]
        return self.x == other.x  # error: [unguarded-operand]
```

## A `NotImplemented` for an operand inside the domain moves on

a `return NotImplemented` reached with an operand the annotation takes hands that one back too, so
the operation also takes whatever python asks next

```py
class Small:
    def __add__(self, other: int) -> int:
        if not isinstance(other, int):
            return NotImplemented
        if other > 1:
            return NotImplemented
        return 0

# error: [unsupported-operator]
Small() + 1
```

## Reassigning the operand

a body that puts another value in the parameter tests that, and not what python handed it

```py
class Rebind:
    def __add__(self, other: int) -> int:  # error: [unguarded-operand] "`__add__` puts another value in `other`"
        other = 1
        return other
```

## A method that takes every operand is not checked

```py
class Anything:
    def __add__(self, other: object) -> int:
        return 1  # no diagnostic

    def __sub__(self, other) -> int:
        return 1  # no diagnostic
```

## An overloaded method is tested against every overload

dispatch asks each overload, so the domain is all of theirs together

```py
from typing import overload

class Mixed:
    @overload
    def __add__(self, other: int) -> int: ...
    @overload
    def __add__(self, other: str) -> str: ...
    def __add__(self, other: int | str) -> int | str:
        if isinstance(other, (int, str)):
            return other
        return NotImplemented  # no diagnostic
```

## A `.by` dunder is lowered with the test

```by
class Lowered:
    def __add__(self, other: int) -> int:
        return 1  # no diagnostic

assert Lowered() + 1 == 1
```

## An operand that inherits its type is checked against it

an unannotated operand takes its type from the method it overrides, and is handed whatever the other
operand is like an annotated one

```py
class Base:
    def __add__(self, other: int) -> int:
        if not isinstance(other, int):
            return NotImplemented
        return 1

class Sub(Base):
    def __add__(self, other):
        reveal_type(other)  # revealed: object
        return 2  # error: [unguarded-operand]
```

an overload implementation with no annotation of its own is checked against its overloads

```py
from typing import overload

class Mixed:
    @overload
    def __add__(self, other: int) -> int: ...
    @overload
    def __add__(self, other: str) -> str: ...
    def __add__(self, other):
        return 1  # error: [unguarded-operand]
```

## A `return` inside `try` is checked where it is written

```py
class Locked:
    def __add__(self, other: int) -> int:
        try:
            return 1  # error: [unguarded-operand]
        finally:
            pass
```

a function defined inside a module's `try` with a `finally` suite returns from that function, which
the suite never runs after

```py
try:
    class Wrapped:
        def __add__(self, other: int) -> int:
            return 1  # error: [unguarded-operand]

finally:
    pass
```

## Raising for an operand outside the domain

python stops at a raise, where the operation was typed as moving on to the other operand, so a raise
reached with an operand that may be outside the domain is reported too. so is an `assert` whose test
can fail

```py
class Raises:
    def __add__(self, other: int) -> int:
        if not isinstance(other, int):
            raise TypeError("not an int")  # error: [unguarded-operand] "where this raises"
        return other

class Asserts:
    def __add__(self, other: int) -> int:
        assert isinstance(other, int)  # error: [unguarded-operand] "where this raises"
        return other

class Fine:
    def __add__(self, other: int) -> int:
        if not isinstance(other, int):
            return NotImplemented
        if other < 0:
            raise ValueError("negative")  # no diagnostic
        return other
```

## Only the operands a `NotImplemented` is reached with move on

a `return NotImplemented` hands back only what the operand can be where it is reached, so the
operation moves on only for those

```py
from typing import final

@final
class Money:
    def __add__(self, other: "Money | int") -> "Money":
        if isinstance(other, int):
            if other == 0:
                return self
            return NotImplemented
        if not isinstance(other, Money):
            return NotImplemented
        return Money()

reveal_type(Money() + Money())  # revealed: Money
# error: [unsupported-operator]
Money() + 1
```

an operand of a class that isn't final may also be an `int`, through a class that inherits from
both, so it may be handed back too

```py
class OpenMoney:
    def __add__(self, other: "OpenMoney | int") -> "OpenMoney":
        if isinstance(other, int):
            if other == 0:
                return self
            return NotImplemented
        if not isinstance(other, OpenMoney):
            return NotImplemented
        return OpenMoney()

# error: [unsupported-operator]
OpenMoney() + OpenMoney()
```

an operand with no annotation takes every operand, and a `NotImplemented` its body returns hands
back whatever reaches it

```py
class Point:
    def __add__(self, other):
        if not isinstance(other, Point):
            return NotImplemented
        return Point()

reveal_type(Point() + Point())  # revealed: Point
# error: [unsupported-operator]
Point() + 1
```

## A protocol's members are not checked

a method a protocol declares is never the one python calls

```py
from typing import Protocol

class SupportsAdd(Protocol):
    def __add__(self, other: int) -> int:
        pass  # no diagnostic
```

## A default is checked against the annotation

```py
class A:
    # error: [invalid-parameter-default]
    def __add__(self, other: int = "s") -> int:
        if not isinstance(other, int):
            return NotImplemented
        return other
```
