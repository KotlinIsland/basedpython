# Empty declarations

basedpython lets a `class` or a `def` be written with no body at all. An empty class is a whole
class — nothing about it is left out. A `def` with no body is a declaration: the lowering fills the
body in with `: ...`, so what runs is a function that returns `None`.

A stub file, a protocol, an `abstract def`, an overload group and an `if TYPE_CHECKING` block each
ask for exactly that. Anywhere else the implementation the signature promises was never written, and
`missing-function-body` reports it.

## A `def` with no body

At module scope, in a class, nested in another function, and in a loop body — a `def` written there
is one the program is going to run:

```by
# error: [missing-function-body]
def lookup() -> int

class C:
    # error: [missing-function-body]
    def m(self) -> int

def outer() -> int:
    # error: [missing-function-body]
    def inner() -> int

    return 1

for _ in range(3):
    # error: [missing-function-body]
    def each() -> int
```

## The return type makes no difference

The body is what went missing, whether or not `None` would have satisfied what the signature says
the function returns. A `def` that declares no return type at all is the case that says nothing
about returning:

```by
# error: [missing-function-body]
def a()

# error: [missing-function-body]
def b() -> int
```

## An empty class body

A class with no body is a class with no members, which is a whole class. Nothing is reported:

```by
class Empty

class Sub(Empty)

reveal_type(Sub())  # revealed: final Sub
```

## A stub file declares

```byi
def f() -> int

class C:
    def m(self) -> str
```

## A protocol member declares

A protocol says what its implementations provide, so a member is a signature and nothing else:

```by
from typing import Protocol

class P(Protocol):
    def m(self) -> int

protocol Q:
    def m(self) -> int

def use(p: P, q: Q) -> int:
    return p.m() + q.m()
```

## A class that inherits from a protocol does not declare

Only a class that inherits from `Protocol` directly is a protocol, so a subclass of one needs bodies
like any other class:

```by
from typing import Protocol

class P(Protocol):
    def m(self) -> int

class Impl(P):
    # error: [missing-function-body]
    def m(self) -> int
```

## An abstract method declares

An `abstract def` is given a `raise NotImplementedError` body, and an `@abstractmethod` a `: ...`
one. Either way the method exists to be overridden:

```by
from abc import ABC, abstractmethod

class A(ABC):
    abstract def m(self) -> int

    @abstractmethod
    def n(self) -> int

class B(A):
    def m(self) -> int:
        return 1

    def n(self) -> int:
        return 2
```

## An overload declaration declares

A run of same-name bodyless `def`s is an overload group: the lowering writes the `@overload`
decorators the source leaves out, and the implementation is the one that carries a body.

```by
def parse(s: str) -> int
def parse(s: bytes) -> int
def parse(s):
    return int(s)

reveal_type(parse("1"))  # revealed: int
```

A written `@overload` declares the same way:

```by
from typing import overload

@overload
def parse(s: str) -> int
@overload
def parse(s: bytes) -> int
def parse(s):
    return int(s)
```

## An `if TYPE_CHECKING` block declares

Nothing in such a block runs, so there is nothing for a body to do:

```by
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    def declared() -> int
```

## `empty-body` is about a body that is there

A body written as `: ...` is a body: the function is meant to do nothing, and only its return type
can be wrong about that. So the two reports never both apply to one `def`:

```by
# error: [empty-body]
def written() -> int: ...

# error: [missing-function-body]
def unwritten() -> int
```

## A `decorator def` needs a body too

The lowering writes a `decorator def` a dispatcher, which calls the body the source is supposed to
supply — so the missing one is missing there as well:

```by
# error: [missing-function-body]
decorator def route(fn: (int) -> None)
```

In a stub file it declares the decorator's shape, like any other declaration:

```byi
decorator def route(fn: (int) -> None)
```

## `init(...)` writes its own body

An `init(...)` has its body built from its parameter list — the attribute parameters it declares are
the whole of what it does — so there is never one left out:

```by
class Point:
    init(let x: int, let y: int)

class Nothing:
    init()

reveal_type(Point(1, 2).x)  # revealed: int
reveal_type(Nothing())  # revealed: final Nothing
```
