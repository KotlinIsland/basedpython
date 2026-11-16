# Operator domains

python calls an operator dunder with whatever the other operand is. the method either rejects the
operand by returning `NotImplemented`, and python moves on to the other operand's reflected dunder
(or from `__iadd__` to `__add__`), or it runs with it

what a method can tell apart is only what a runtime check can see: the operand's class, not its type
arguments. so the parameter's type with its type arguments taken off — its runtime domain — is the
set of operands the method runs with. an operand outside the domain is rejected, and an operand
inside it that the parameter doesn't accept is a type error with no fallback

```toml
[environment]
python-version = "3.12"
```

## A rejected operand reaches the reflected dunder

```py
class A:
    def __add__(self, other: int) -> int:
        return 1

class B:
    def __radd__(self, other: A) -> str:
        return "b"

reveal_type(A() + B())  # revealed: str
```

## An in-place dunder that rejects falls back to the binary dunder

`__iadd__` rejects a `str`, so python calls `__add__`, and the target is rebound to its result

```py
class Foo:
    def __iadd__(self, other: int) -> int:
        return 1

    def __add__(self, other: str) -> str:
        return "pear"

foo = Foo()
foo += "a"
reveal_type(foo)  # revealed: str

bar = Foo()
bar += 1
reveal_type(bar)  # revealed: int
```

## An in-place dunder runs with an operand inside its domain

`list.__iadd__` takes an `Iterable[T]`. a `list[str]` is an `Iterable`, so the method can't reject
it and extends the list in place. falling back to `list.__add__` would give `xs` a new type while
`ys` still holds the same list

```py
xs = [1, 2, 3]
ys = xs
# error: [unsupported-operator]
xs += ["a"]
```

## A binary dunder runs with an operand inside its domain

`V[int].__add__` can't tell a `V[str]` from a `V[int]`, so it runs with it, and `W.__radd__` is
never reached

```py
class V[T]:
    def __add__(self, other: "V[T]") -> "V[T]":
        return self

    def __radd__(self, other: object) -> str:
        return ""

def f(a: V[int], b: V[str]):
    # error: [unsupported-operator]
    a + b
```

## Overloads reject only what no overload's domain covers

```py
from typing import overload

class C:
    @overload
    def __add__(self, other: int) -> int: ...
    @overload
    def __add__(self, other: list[int]) -> str: ...
    def __add__(self, other: int | list[int]) -> int | str:
        return 1

class R:
    def __radd__(self, other: C) -> bytes:
        return b""

def f(c: C, r: R, strings: list[str]):
    reveal_type(c + 1)  # revealed: int
    reveal_type(c + r)  # revealed: bytes
    # error: [unsupported-operator]
    c + strings
```

## Each possible receiver rejects on its own

a value typed `int | float` is one or the other. when it's an `int`, `int.__add__` rejects the
`float` and python calls `float.__radd__`

```py
def f(x: int | float):
    reveal_type(x + 1.0)  # revealed: float
```

## A comparison that runs with its operand doesn't fall back

```py
class V[T]:
    def __lt__(self, other: "V[T]") -> bool:
        return True

    def __gt__(self, other: object) -> bool:
        return True

def f(a: V[int], b: V[str]):
    # error: [unsupported-operator]
    a < b
```

an `__eq__` that runs with its operand doesn't fall back to identity either

```py
class E[T]:
    def __eq__(self, other: "E[T]") -> bool:  # error: [invalid-method-override]
        return True

def g(a: E[int], b: E[str]):
    # error: [unsupported-operator]
    a == b
```

## A dunder whose return type includes `NotImplementedType` may still reject

a dunder can hand back an operand of the right class too, by testing its value. it says so in the
return type it writes, and the operation then also takes whatever python asks next

```py
from types import NotImplementedType

class Number:
    value: int = 0

class Small:
    def __add__(self, other: Number) -> int | NotImplementedType:
        if other.value > 1:
            return NotImplemented
        return 0

class Count(Number):
    def __radd__(self, other: Small) -> str:
        return ""

reveal_type(Small() + Count())  # revealed: int | str
```

when nothing after it takes the operand, python raises a `TypeError`

```py
from types import NotImplementedType

class Small:
    def __add__(self, other: int) -> int | NotImplementedType:
        return NotImplemented

# error: [unsupported-operator]
Small() + 1
```

python asks the reflected dunder only of an operand of another type

```py
from types import NotImplementedType

class Same:
    def __add__(self, other: "Same") -> "Same | NotImplementedType":
        return NotImplemented

# error: [unsupported-operator]
Same() + Same()
```

## An in-place dunder that may reject falls back to the binary dunder

```py
from types import NotImplementedType

class Counter:
    def __iadd__(self, other: int) -> int | NotImplementedType:
        return NotImplemented

    def __add__(self, other: int) -> str:
        return ""

counter = Counter()
counter += 1
reveal_type(counter)  # revealed: int | str
```

## A comparison that may reject falls through

```py
from types import NotImplementedType

class Order:
    def __lt__(self, other: "Order") -> bool | NotImplementedType:
        return NotImplemented

    def __gt__(self, other: "Order") -> bool | NotImplementedType:
        return NotImplemented

# error: [unsupported-operator]
Order() < Order()
```

## An inferred return type never reaches the operation

a return type inferred from the body also counts a `return NotImplemented` that rejects an operand
of the wrong class, which dispatch already moves past. the operation never has `NotImplementedType`
in its type

```py
class A:
    def __add__(self, other):
        if not isinstance(other, A):
            return NotImplemented
        return A()

reveal_type(A() + A())  # revealed: A
```

## A `.by` function declares the `NotImplemented` it returns

in a `.by` file `NotImplemented` is checked like any other value, so it has to be in the declared
return type

```by
class A:
    def __add__(self, other: int) -> int:
        # error: [invalid-return-type]
        return NotImplemented
```

```by
from types import NotImplementedType

class B:
    def __add__(self, other: int) -> int | NotImplementedType:
        return NotImplemented  # no diagnostic
```

## A `.by` dunder's operand is tested before its body runs

a `.by` operator dunder hands back an operand outside its parameter's domain itself: the lowering
tests the operand before the body runs. that needs a test the runtime can make, and an operand
annotated with a type it can't test an operand against is reported

```by
from typing import Protocol, SupportsIndex

class HasX(Protocol):
    x: int

class A:
    # error: [erased-type-check] "`is HasX` cannot be checked at runtime: the test can't tell that an operand is outside it"
    def __add__(self, other: HasX) -> int:
        return other.x

    def __sub__(self, other: SupportsIndex) -> int:  # no diagnostic
        return other.__index__()

    def __mul__(self, other: (int) -> str) -> str:  # no diagnostic
        return other(1)
```

a class the module binds only under `if TYPE_CHECKING:` isn't there for the test to read

```by
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from decimal import Decimal

class C:
    # error: [erased-type-check]
    def __add__(self, other: Decimal) -> int:
        return 0
```

## A `.by` dunder that takes every operand tests nothing

an operand of any type needs no test, and neither does a method python never moves past

```by
from typing import Protocol

class HasX(Protocol):
    x: int

class B:
    def __add__(self, other: object) -> int:  # no diagnostic
        return 0

    def __contains__(self, item: HasX) -> bool:  # no diagnostic
        return True
```

## A `.by` dunder runs with every operand it has no annotation for

the type of a `.by` parameter written without one comes from what the body asks of it, which isn't
something a test is written from. the method runs with every operand, so one it doesn't take is an
error rather than handed back

```by
class A:
    def __add__(self, other):
        return other.real

class B:
    def __radd__(self, other: A) -> str:
        return ""

A() + 1  # no diagnostic
# error: [unsupported-operator]
A() + B()
```
