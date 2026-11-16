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

## An `__eq__` or `__ne__` override may narrow its operand

python hands `__eq__` every operand, and one outside its domain moves on to the other operand's
`__eq__` and then to an identity test. so an `__eq__` that takes only some operands still compares
with every one, and still answers with a `bool`. that makes it a valid override of
`object.__eq__(self, other: object) -> bool`, wherever the method really hands that operand back: in
a `.py` file, that is where the project checks that it does

```toml
[analysis]
checked-operator-operands = true
```

```py
class Point:
    def __eq__(self, other: "Point") -> bool:  # no diagnostic
        if not isinstance(other, Point):
            return NotImplemented
        return True

    def __ne__(self, other: "Point") -> bool:  # no diagnostic
        if not isinstance(other, Point):
            return NotImplemented
        return False

reveal_type(Point() == Point())  # revealed: bool
reveal_type(Point() == 1)  # revealed: bool
reveal_type(Point() != 1)  # revealed: bool
```

an override of an `__eq__` that narrows may narrow further

```py
class Shape:
    def __eq__(self, other: "Shape") -> bool:
        if not isinstance(other, Shape):
            return NotImplemented
        return True

class Square(Shape):
    def __eq__(self, other: "Square") -> bool:  # no diagnostic
        if not isinstance(other, Square):
            return NotImplemented
        return True
```

## An unchecked `.py` `__eq__` may not narrow its operand

without `checked-operator-operands`, nothing holds a `.py` `__eq__` to handing back what it doesn't
take, so `Point() == 1` may run its body with the `int`

```py
class Point:
    x: int = 0

    def __eq__(self, other: "Point") -> bool:  # error: [invalid-method-override]
        return self.x == other.x
```

## A `.by` `__eq__` that narrows its operand compares with every operand

the `.by` override tests its operand before its body runs, so an `int` never reaches `other.x`

```by
class Point:
    x: int = 0

    def __eq__(self, other: Point) -> bool:  # no diagnostic
        return self.x == other.x

assert Point() == Point()
assert not (Point() == 1)
assert Point() != 1
```

## An `__eq__` override may hand back an operand by value

handing back an operand of the right class moves on the same way, so writing `NotImplementedType` in
the return type is a valid override too

```toml
[analysis]
checked-operator-operands = true
```

```py
from types import NotImplementedType

class Money:
    cents: int = 0

    def __eq__(self, other: object) -> bool | NotImplementedType:  # no diagnostic
        if not isinstance(other, Money):
            return NotImplemented
        return self.cents == other.cents
```

## An `__eq__` override that narrows its operand still answers with a `bool`

an operand the override hands back is answered with a `bool`, so an override that narrows has to
answer with something a `bool` is assignable to wherever the method it overrides does

```toml
[analysis]
checked-operator-operands = true
```

```py
class Base:
    def __eq__(self, other: object) -> int:  # error: [invalid-method-override]
        return 0

class Sub(Base):
    def __eq__(self, other: "Sub") -> int:  # no diagnostic
        if not isinstance(other, Sub):
            return NotImplemented
        return 0

class Text:
    def __eq__(self, other: "Text") -> str:  # error: [invalid-method-override]
        if not isinstance(other, Text):
            return NotImplemented
        return ""
```

## Other comparisons may not narrow their operand

an operand that `<` hands back and nothing else takes is a `TypeError`, not an answer, so a `__lt__`
override that narrows its operand changes what the method it overrides promised

```py
class Base:
    def __lt__(self, other: object) -> bool:
        return True

class Sub(Base):
    def __lt__(self, other: "Sub") -> bool:  # error: [invalid-method-override]
        return True
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

## A class's type parameter is not part of the domain

an operand annotated with the class's type parameter is tested for what the parameter is bound to,
since the receiver's type arguments are nothing a runtime test can see. `Box[int]` runs `__iadd__`
with a `str`

```py
class Box[T]:
    def __iadd__(self, other: T) -> "Box[T]":
        return self

    def __add__(self, other: object) -> str:
        return ""

def f(box: Box[int]):
    # error: [unsupported-operator]
    box += "s"
```

## A union operand reaches the method if any of its types is inside the domain

```py
class Box[T]:
    pass

class A:
    def __iadd__(self, other: Box[int]) -> "A":
        return self

    def __add__(self, other: object) -> str:
        return ""

def f(a: A, value: Box[str] | int):
    # error: [unsupported-operator]
    a += value
```

## Only a method that tests its operand hands it back

a coroutine or generator function hands back the coroutine or generator it makes, whatever it is
written with. and the test a `.by` dunder is lowered with, or a checked `.py` one is written with,
is only at the top of a plain `def` of the dunder: a function bound to a dunder under another name
is called with whatever the operand is, and so is a decorated method, since python calls whatever
the decorator made. none of them move on to the other operand

```toml
[environment]
python-version = "3.13"

[analysis]
checked-operator-operands = true
```

```py
from typing import Callable

class Other:
    def __radd__(self, other: object) -> str:
        return ""

def keep[**P, R](f: Callable[P, R]) -> Callable[P, R]:
    return f

def add(self: "Aliased", other: int) -> int:
    return 1

class Async:
    async def __add__(self, other: int) -> int:
        return 1

class Generator:
    def __add__(self, other: int):
        yield 1

class Aliased:
    __add__ = add

class Decorated:
    @keep
    def __add__(self, other: int) -> int:
        return 1

class Variadic:
    def __add__(self, *others: int) -> int:
        return 1

# error: [unsupported-operator]
Async() + Other()
# error: [unsupported-operator]
Generator() + Other()
# error: [unsupported-operator]
Aliased() + Other()
# error: [unsupported-operator]
Decorated() + Other()
# error: [unsupported-operator]
Variadic() + Other()
```

## A call that fails for another reason hands nothing back

```py
class Other:
    def __rsub__(self, other: object) -> str:
        return ""

class A:
    def __sub__(self, other: int, extra: int) -> int:
        return 1

# error: [unsupported-operator]
A() - Other()
```

## An unannotated operand takes every operand

```py
class Other:
    x: int = 0

    def __radd__(self, other: object) -> str:
        return ""

class Point:
    x: int = 0

class A:
    def __add__(self, other):
        return other.x + 1

A() + Point()  # no diagnostic
# error: [unsupported-operator]
A() + 1
```

a method whose inferred return type is only `NotImplementedType` hands back whatever it takes

```py
class Never:
    def __add__(self, other):
        return NotImplemented

# error: [unsupported-operator]
Never() + 1
```

## A type alias doesn't hide `NotImplementedType`

```py
from types import NotImplementedType

type Answer = int | NotImplementedType

class A:
    def __add__(self, other: int) -> Answer:
        return NotImplemented

# error: [unsupported-operator]
A() + 1
```

## A `.by` protocol's dunders are not tested

a method a protocol declares is never the one python calls, so its operand needs no runtime test

```by
from typing import Protocol

class HasX(Protocol):
    x: int

class SupportsAdd(Protocol):
    def __add__(self, other: HasX) -> int: ...  # no diagnostic
```
