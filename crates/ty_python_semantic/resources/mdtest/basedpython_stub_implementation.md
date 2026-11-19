# basedpython: a runtime module is checked against its stub

a module that has a stub is typed by the stub wherever it is imported, so what the stub declares is
what the module promises its users. its own body is held to the same promise: what the stub declares
for something the module leaves unannotated is the declared type there.

```toml
[rules]
missing-stub-member = "ignore"
```

## a parameter takes the type the stub declares

`m.pyi`:

```pyi
def f(a: int, b: str = ...) -> None: ...
```

`m.py`:

```py
def f(a, b="b"):
    reveal_type(a)  # revealed: int
    reveal_type(b)  # revealed: str

reveal_type(f)  # revealed: def f(a: int, b: str = "b")

f(1)
f("a")  # error: [invalid-argument-type]
```

importers read the stub, as they did before:

```py
from m import f

reveal_type(f)  # revealed: def f(a: int, b: str = ...)
```

## an explicit annotation wins

the runtime module's own annotation is what its body is checked against. where it disagrees with the
stub, that is the stub consistency rules' to report.

`m.pyi`:

```pyi
def f(a: int) -> None: ...
```

`m.py`:

```py
def f(a: object):
    reveal_type(a)  # revealed: object
```

## a method's parameters

the receiver is the runtime class's own, and the other parameters take the stub's types.

`m.pyi`:

```pyi
class A:
    def f(self, a: int) -> None: ...
    @classmethod
    def g(cls, a: str) -> None: ...
    @staticmethod
    def h(a: bytes) -> None: ...
```

`m.py`:

```py
class A:
    def f(self, a):
        reveal_type(self)  # revealed: Self@f
        reveal_type(a)  # revealed: int

    @classmethod
    def g(cls, a):
        reveal_type(a)  # revealed: str

    @staticmethod
    def h(a):
        reveal_type(a)  # revealed: bytes
```

## the body's returns are checked against the stub's return type

`m.pyi`:

```pyi
def f(a: int) -> str: ...
def g() -> int: ...
```

`m.py`:

```py
def f(a):
    return a  # snapshot

def g():  # error: [invalid-return-type] "Function always implicitly returns `None`, which is not assignable to return type `int`"
    print("no return")

reveal_type(f(1))  # revealed: str
```

```snapshot
error[invalid-return-type]: Return type does not match returned value
 --> src/m.py:2:12
  |
1 | def f(a):
  |     - Expected `str` because of return type
2 |     return a  # snapshot
  |            ^ expected `str`, found `int`
  |
 ::: src/m.pyi:1:18
  |
1 | def f(a: int) -> str: ...
  |                  --- Return type declared by the stub here
```

## an overloaded function

the one runtime `def` implements every overload, as the implementation of overloads written in one
file does: a parameter is the union of what each overload says, and so is the return type.

`m.pyi`:

```pyi
from typing import overload

@overload
def f(a: int) -> int: ...
@overload
def f(a: str, b: bytes) -> str: ...
```

`m.py`:

```py
def f(a, b=b""):
    reveal_type(a)  # revealed: int | str
    reveal_type(b)  # revealed: bytes
    return a
```

## a `sys.version_info` branch

only the stub's declaration in the branch that runs is read.

```toml
[environment]
python-version = "3.12"
```

`m.pyi`:

```pyi
import sys

if sys.version_info >= (3, 12):
    def f(a: int) -> None: ...

else:
    def f(a: str) -> None: ...
```

`m.py`:

```py
def f(a):
    reveal_type(a)  # revealed: int
```

## the stub's own classes are the module's

a class the stub defines describes the runtime module's class of the same name, so a type that names
it is read as naming the runtime class: a body that takes one, or returns one, is checked against
the class it actually defines.

`m.pyi`:

```pyi
class Node:
    def link(self, other: Node) -> Node: ...

def make() -> Node: ...
def wrap(nodes: list[Node], kind: type[Node]) -> tuple[Node, ...]: ...
```

`m.py`:

```py
class Node:
    def link(self, other):
        reveal_type(other)  # revealed: Node
        return other

def make():
    return Node()

def wrap(nodes, kind):
    reveal_type(nodes)  # revealed: list[Node]
    reveal_type(kind)  # revealed: type[Node]
    return tuple(nodes)

def broken():
    return make().missing  # error: [unresolved-attribute]

reveal_type(make())  # revealed: Node
```

a class nested in another is found the same way:

`n.pyi`:

```pyi
class Outer:
    class Inner: ...

    def inner(self) -> Inner: ...
```

`n.py`:

```py
class Outer:
    class Inner: ...

    def inner(self):
        return 1  # error: [invalid-return-type] "Return type does not match returned value: expected `Inner`, found `Literal[1]`"
```

## a stub class with no runtime counterpart is not carried

a class the stub defines and the module does not (often a protocol the stub declares only for
itself) means nothing in the module, so a type naming it is not carried, and the parameter is left
as it would be without the stub: here, an anonymous type parameter of its own.

`m.pyi`:

```pyi
from typing import Protocol, type_check_only

@type_check_only
class _SupportsClose(Protocol):
    def close(self) -> None: ...

def shut(a: _SupportsClose, b: int) -> None: ...
```

`m.py`:

```py
def shut(a, b):
    reveal_type(a)  # revealed: a@shut
    reveal_type(b)  # revealed: int
```

## `Self` is the runtime method's

`m.pyi`:

```pyi
from typing import Self

class Builder:
    def copy(self) -> Self: ...
    def merge(self, other: Self) -> Self: ...
```

`m.py`:

```py
class Builder:
    def copy(self):
        reveal_type(self)  # revealed: Self@copy
        return self

    def merge(self, other):
        reveal_type(other)  # revealed: Self@merge
        return Builder()  # error: [invalid-return-type]

class Sub(Builder): ...

reveal_type(Sub().copy())  # revealed: Sub
```

## a generic function's type variables are the runtime function's

the runtime function becomes generic over the stub function's type variables: its body sees them as
type parameters, and its callers in the module solve them.

```toml
[environment]
python-version = "3.12"
```

`m.pyi`:

```pyi
from typing import TypeVar

T = TypeVar("T")

def first(items: list[T]) -> T: ...
def same[U](a: U, b: U) -> U: ...
```

`m.py`:

```py
def first(items):
    reveal_type(items)  # revealed: list[T@first]
    return items[0]

def same(a, b):
    reveal_type(a)  # revealed: U@same
    return 1  # error: [invalid-return-type]

reveal_type(first([1, 2]))  # revealed: int
reveal_type(same("a", "b"))  # revealed: Literal["a", "b"]
```

a type variable bounded by one of the stub's classes is bounded by the runtime module's:

`n.pyi`:

```pyi
from typing import TypeVar

class Action: ...

A = TypeVar("A", bound=Action)

def register(action: A) -> A: ...
```

`n.py`:

```py
class Action: ...

def register(action):
    return action

reveal_type(register(Action()))  # revealed: Action
register(1)  # error: [invalid-argument-type]
```

## a generic class's type parameters are the runtime class's

a type variable a stub class binds is the runtime class's type parameter in the same position.

`m.pyi`:

```pyi
from typing import Generic, TypeVar

T = TypeVar("T")

class Box(Generic[T]):
    def get(self) -> T: ...
    def put(self, item: T) -> None: ...
    def copy(self) -> Box[T]: ...
```

`m.py`:

```py
from typing import Generic, TypeVar

T = TypeVar("T")

class Box(Generic[T]):
    def get(self):
        return 1  # error: [invalid-return-type]

    def put(self, item):
        reveal_type(item)  # revealed: T@Box

    def copy(self):
        return self

reveal_type(Box[int]().copy())  # revealed: Box[int]
```

where the runtime class is not generic, it has no type parameter for the stub's to be, so a type
naming one is not carried:

`n.pyi`:

```pyi
from typing import Generic, TypeVar

T = TypeVar("T")

class Box(Generic[T]):
    def put(self, item: T, count: int) -> None: ...
```

`n.py`:

```py
class Box:
    def put(self, item, count):
        reveal_type(item)  # revealed: item@put
        reveal_type(count)  # revealed: int
```

## a module variable takes the type the stub declares

`m.pyi`:

```pyi
X: int
Y: str
```

`m.py`:

```py
X = 1
X = "a"  # error: [invalid-assignment]
Y = "y"
reveal_type(Y)  # revealed: Literal["y"]

def f():
    reveal_type(Y)  # revealed: str
```

## a class attribute takes the type the stub declares

`m.pyi`:

```pyi
from typing import ClassVar

class A:
    x: int
    y: ClassVar[str]
```

`m.py`:

```py
class A:
    y = "y"

    def __init__(self):
        self.x = 1

    def reset(self):
        self.x = "a"  # error: [invalid-assignment]

reveal_type(A().x)  # revealed: int
reveal_type(A.y)  # revealed: str
A.y = 1  # error: [invalid-assignment]
```
