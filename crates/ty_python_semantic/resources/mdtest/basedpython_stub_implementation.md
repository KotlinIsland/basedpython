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

## types only the stub has are not carried

a class the stub defines is not the runtime module's class of the same name, and a type variable the
stub binds is bound to the stub's declaration, so a type that names either says nothing the runtime
body could be held to. such a parameter is left as it would be without the stub: here, an anonymous
type parameter of its own.

`m.pyi`:

```pyi
from typing import TypeVar

T = TypeVar("T")

class A: ...

def f(a: A, b: int) -> A: ...
def g(a: T) -> T: ...
```

`m.py`:

```py
class A: ...

def f(a, b):
    reveal_type(a)  # revealed: a@f
    reveal_type(b)  # revealed: int
    return A()

def g(a):
    reveal_type(a)  # revealed: a@g
    return a
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
