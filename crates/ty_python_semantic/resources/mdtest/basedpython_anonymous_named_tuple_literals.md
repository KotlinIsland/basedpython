# basedpython: a plain tuple literal built as an anonymous named tuple

a plain tuple literal standing where an [anonymous named tuple](anonymous_named_tuple.md) is
expected is an instance of it, and the transpiler constructs that class around it, so the fields the
checker accepts reading are there at runtime. every block here runs

## Every position that gives the literal an expected type

the expectation can come from anywhere bidirectional inference reaches: an argument, a keyword
argument, an element of `*args`, a default, a `yield`, an element or value of a collection literal
at any depth, a method call on a collection, or a write to an attribute. each value below has its
fields at runtime

```by
from typing import Iterator

def take(p: (a: int, b: int)) -> int:
    return p.a

def keyword(*, p: (a: int, b: int)) -> int:
    return p.b

def star(*ps: (a: int, b: int)) -> int:
    return sum(p.a for p in ps)

def default(p: (a: int, b: int) = reveal_type((5, 6))) -> int:  # revealed: (a: int, b: int)
    return p.a

def pairs() -> Iterator[(a: int, b: int)]:
    yield reveal_type((1, 2))  # revealed: (a: int, b: int)

class Box:
    var pair: (a: int, b: int) = (0, 0)

assert take(reveal_type((7, 8))) == 7  # revealed: (a: int, b: int)
assert keyword(p=reveal_type((7, 8))) == 8  # revealed: (a: int, b: int)
assert star(reveal_type((1, 2)), (3, 4)) == 4  # revealed: (a: int, b: int)
assert default() == 5
assert next(pairs()).b == 2

items: list[(a: int, b: int)] = []
items.append(reveal_type((1, 2)))  # revealed: (a: int, b: int)
assert items[0].a == 1

by_name: dict[str, (a: int, b: int)] = {"k": reveal_type((1, 2))}  # revealed: (a: int, b: int)
assert by_name["k"].b == 2

nested: list[list[(a: int, b: int)]] = [[reveal_type((1, 2))]]  # revealed: (a: int, b: int)
assert nested[0][0].a == 1

box = Box()
box.pair = reveal_type((3, 4))  # revealed: (a: int, b: int)
assert box.pair.b == 4
```

where a plain tuple type is expected, a literal stays a plain tuple:

```by
t: tuple[int, int] = reveal_type((3, 4))  # revealed: (3, 4)
```

## A field that is itself an anonymous named tuple

the literal for such a field is expected as that field's type, so it is built as the inner class:

```by
def outer(p: (inner: (x: int, y: int), label: str)) -> int:
    return p.inner.y

assert outer(((1, 2), "a")) == 2
```

## One arm of a union

an optional anonymous named tuple builds the literal as its one anonymous named tuple arm:

```by
maybe: (a: int) | None = reveal_type((1,))  # revealed: (a: int)
assert maybe.a == 1
```

where the expected type allows several anonymous named tuples with as many fields as the literal
has, the one whose field types accept its elements is built:

```by
either: (a: int) | (b: str) = reveal_type(("x",))  # revealed: (b: str)
assert either.b == "x"

other: (a: int) | (b: str) = reveal_type((1,))  # revealed: (a: int)
assert other.a == 1
```

an anonymous named tuple arm that does not accept the literal leaves it a plain tuple, inferred
against the rest of the expected type:

```by
loose: (a: list[int]) | tuple[list[object]] = ([1, "s"],)
assert loose == ([1, "s"],)
```

an anonymous named tuple is a subtype of the plain tuple of its field types, so a union with that
tuple is the tuple alone, and the literal stays a plain tuple:

```by
plain: (a: int) | tuple[int] = reveal_type((1,))  # revealed: (1,)
assert type(plain).__name__ == "tuple"
```

## A literal more than one arm accepts is an error

the class decides which field names the value has at runtime, and nothing in a plain tuple says
which one was meant, so a literal more than one anonymous named tuple accepts is reported rather
than built as whichever the union lists first:

```by
# error: [ambiguous-conversion] "More than one anonymous named tuple accepts this tuple"
either: (a: int) | (b: int) = (1,)
```

## A write through a union receiver whose members agree

one value reaches whichever class the receiver turns out to be. where each declares the same
anonymous named tuple, the literal is built as it:

```by
class Left:
    var pair: (a: int, b: int) = (0, 0)

class Right:
    var pair: (a: int, b: int) = (0, 0)

def assign(target: Left | Right) -> None:
    target.pair = reveal_type((1, 2))  # revealed: (a: int, b: int)
    assert target.pair.a == 1

assign(Left())
assign(Right())
```

## A write through a union receiver whose members build the same class

the members need not declare the same type, only build the same class: one that may also hold `None`
builds the literal as the same anonymous named tuple

```by
type P = (a: int, b: int)

class First:
    var pair: P = (0, 0)

class Second:
    var pair: P = (0, 0)

class Optional:
    var pair: P | None = None

def assign(target: First | Second | Optional) -> None:
    target.pair = (1, 2)
    assert target.pair.b == 2

assign(First())
assign(Optional())
```

## A write through a union receiver one of whose members builds nothing

a member that builds nothing from the literal accepts the class another builds, which is a subtype
of the plain tuple:

```by
type P = (a: int, b: int)

class Built:
    var pair: P = (0, 0)

class Loose:
    var pair: object = 0

def loosen(target: Built | Loose) -> None:
    target.pair = (1, 2)
    assert target.pair == (1, 2)

loosen(Built())
loosen(Loose())
```

## A write through a union receiver whose members disagree

where the members declare different anonymous named tuples, no single value is both, so the write is
rejected:

```by
class Left:
    var pair: (a: int, b: int) = (0, 0)

class Other:
    var pair: (c: int, d: int) = (0, 0)

def assign(target: Left | Other) -> None:
    # error: [invalid-assignment] "The members of `Left | Other` would build different anonymous named tuples from this value for attribute `pair`"
    target.pair = (1, 2)
```

## A write through a property setter

a property's setter takes the value, so a literal written to the property is built as what the
setter's parameter declares

```by
type Q = (c: int, d: int)

class Store:
    var held: Q = (0, 0)

    @property
    def pair(self) -> Q:
        return self.held

    @pair.setter
    def pair(self, value: Q):
        self.held = value

store = Store()
store.pair = (1, 2)
assert store.pair.d == 2
```

and a receiver that may be either a class declaring one anonymous named tuple or one whose setter
takes another would build two different classes:

```by
type P = (a: int, b: int)

class Declared:
    var pair: P = (0, 0)

class Through:
    var held: Q = (0, 0)

    @property
    def pair(self) -> Q:
        return self.held

    @pair.setter
    def pair(self, value: Q):
        self.held = value

def assign(target: Declared | Through) -> None:
    # error: [invalid-assignment] "The members of `Declared | Through` would build different anonymous named tuples from this value for attribute `pair`"
    target.pair = (1, 2)
```

## A write through a union receiver of other differing declarations

a value with no plain tuple literal for the members to build differently is checked against each
member's own declaration, as python is, so a literal each of them accepts in its own way is fine:

```by
from typing import TypedDict, NotRequired

class Narrow(TypedDict):
    a: int

class Wide(TypedDict):
    a: int
    b: NotRequired[str]

class Ints:
    items: list[int] = []
    cfg: Narrow = {"a": 0}

class Mixed:
    items: list[int | str] = []
    cfg: Wide = {"a": 0}

def assign(target: Ints | Mixed) -> None:
    target.items = [1]
    target.cfg = {"a": 1}
    assert target.items == [1]

assign(Ints())
assign(Mixed())
```

## A `.py` file keeps a plain tuple a tuple

a `.py` file is python, where a plain tuple is never a named tuple, so passing one where a `.by`
module expects an anonymous named tuple is an error there:

`shapes.by`:

```by
def take(p: (a: int, b: int)) -> int:
    return p.a
```

`main.py`:

```py
from shapes import take

take((1, 2))  # error: [invalid-argument-type]
```
