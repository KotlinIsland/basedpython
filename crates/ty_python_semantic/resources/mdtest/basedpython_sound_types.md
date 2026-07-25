# basedpython: sound types

python's gradual guarantee requires a checker to fall back to a gradual type whenever an annotation
is missing, even when a precise type could be inferred. in a fully typed project that is pure
boilerplate — it forces an annotation for something the checker already knows.

the `analysis.sound-types` option deliberately breaks that guarantee and uses the precise type
instead. an explicit annotation always wins over anything inferred here.

this is a basedpython enhancement that also applies to plain python files.

## parameter defaults

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

the parameter's type inside the body is the promoted type of the default:

```py
def f(a=1):
    reveal_type(a)  # revealed: int

def g(a="s", b=True):
    reveal_type(a)  # revealed: str
    reveal_type(b)  # revealed: bool
```

the signature is checked at call sites: an incompatible argument is now an error:

```py
f(2)  # ok
f("x")  # error: [invalid-argument-type]

g("hello", False)  # ok
g(1, True)  # error: [invalid-argument-type]
```

a parameter with no default is unaffected and stays gradual:

```py
def h(a, b=1):
    reveal_type(a)  # revealed: Unknown
    reveal_type(b)  # revealed: int

h("anything", 2)  # ok
```

an explicit annotation always wins over the default:

```py
def annotated(a: str = "s"):
    reveal_type(a)  # revealed: str

annotated("t")  # ok
annotated(1)  # error: [invalid-argument-type]
```

## lambda parameter defaults

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

a lambda parameter with a default follows the same rule as a function parameter, and the lambda's
own signature is checked at its call sites:

```py
g = lambda a=1: a
reveal_type(g)  # revealed: (a: int = 1) -> int

g(2)  # ok
g("x")  # error: [invalid-argument-type]
```

a `Callable` type context still takes priority over the default:

```py
from typing import Callable

cb: Callable[[str], str] = lambda a="s": a
reveal_type(cb)  # revealed: (a: str = "s") -> str
```

## unannotated overrides

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

an unannotated method inherits the parameter and return types of the method it overrides:

```py
class Base:
    def m(self, a: int, b: str = "x") -> bytes:
        return b""

class Sub(Base):
    def m(self, a, b="y"):
        reveal_type(a)  # revealed: int
        reveal_type(b)  # revealed: str
        return b""

reveal_type(Sub().m)  # revealed: bound method Sub.m(a: int, b: str = "y") -> bytes

Sub().m(1)  # ok
Sub().m("nope")  # error: [invalid-argument-type]
```

the lookup starts *after* the class itself, so it finds the overridden method rather than the method
being defined. a method that overrides nothing stays gradual:

```py
class Standalone:
    def m(self, a):
        reveal_type(a)  # revealed: Unknown
```

an explicit annotation on the override always wins:

```py
class Explicit(Base):
    def m(self, a: str, b: str = "y") -> bytes:  # error: [invalid-method-override]
        reveal_type(a)  # revealed: str
        return b""
```

## protocol and abstract members

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

`Protocol` members and `abstractmethod` declarations are ordinary base methods for this purpose:

```py
from typing import Protocol
from abc import ABC, abstractmethod

class P(Protocol):
    def run(self, a: int) -> str: ...

class Impl(P):
    def run(self, a):
        reveal_type(a)  # revealed: int
        return ""

class A(ABC):
    @abstractmethod
    def go(self, x: bytes) -> None: ...

class B(A):
    def go(self, x):
        reveal_type(x)  # revealed: bytes
```

## bare `ClassVar`

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

a bare `ClassVar` uses the inferred type, the same way an unannotated class-body assignment already
does. without this, adding `ClassVar` — a strengthening of intent — would *degrade* the type:

```py
from typing import ClassVar

class C:
    x: ClassVar = 1
    y = 1

reveal_type(C.x)  # revealed: int
reveal_type(C.y)  # revealed: int
```

## unsolved type variables

```toml
[environment]
python-version = "3.13"

[analysis]
sound-types = true
```

a type variable that a call leaves entirely unsolved is solved to `Never` — the precise type of "no
value ever reaches this position" — rather than the gradual `Unknown`:

```py
class Box[T]:
    def __init__(self, *values: T) -> None: ...

reveal_type(Box())  # revealed: Box[Never]
reveal_type(Box(1))  # revealed: Box[Literal[1]]
```

a pep 696 default still takes priority over `Never`:

```py
class Defaulted[T = str]:
    def __init__(self, *values: T) -> None: ...

reveal_type(Defaulted())  # revealed: Defaulted[str]
```

an empty collection literal has element type `Never`, so passing one straight to a generic call
solves from it precisely instead of leaking `Unknown`. a non-empty literal is unaffected:

```py
def first[T](xs: list[T]) -> T:
    return xs[0]

reveal_type(first([]))  # revealed: Never
reveal_type(first([1]))  # revealed: int
```

## disabled (default)

```toml
[environment]
python-version = "3.12"
```

with the option off, the gradual guarantee holds throughout.

```py
from typing import ClassVar

def f(a=1):
    reveal_type(a)  # revealed: Unknown | Literal[1]

f("x")  # ok

g = lambda a=1: a
reveal_type(g)  # revealed: (a=1) -> Unknown | Literal[1]

class Base:
    def m(self, a: int) -> bytes:
        return b""

class Sub(Base):
    def m(self, a):
        reveal_type(a)  # revealed: Unknown
        return b""

class C:
    x: ClassVar = 1

reveal_type(C.x)  # revealed: Unknown | Literal[1]

class Box[T]:
    def __init__(self, *values: T) -> None: ...

reveal_type(Box())  # revealed: Box[Unknown]
```
