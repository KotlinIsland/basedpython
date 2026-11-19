# basedpython: where a module and its stub disagree

importers of a module that has a stub are typed by the stub, so a module that does not do what its
stub declares fails at runtime in code that type-checked. these rules report where the two disagree,
in the module.

## a member the stub declares and the module does not define

`m.pyi`:

```pyi
from typing import TypeVar, TypeAlias, type_check_only

_T = TypeVar("_T")
Alias: TypeAlias = int

def f() -> None: ...
def g() -> None: ...
def _private() -> None: ...
@type_check_only
def checker_only() -> None: ...

class A:
    def m(self) -> None: ...
    def n(self) -> None: ...
```

`m.py`:

```py
def f():  # error: [missing-runtime-member] "`g` is declared by the stub but not defined by this module"
    pass

class A:  # snapshot
    def m(self):
        pass
```

```snapshot
warning[missing-runtime-member]: `n` is declared by the stub but not defined by class `A`
  --> src/m.py:4:7
   |
 4 | class A:  # snapshot
   |       ^
   |
  ::: src/m.pyi:14:9
   |
14 |     def n(self) -> None: ...
   |         - declared here
```

## a member the module defines in a way the stub does not show

a member the module binds through an import, one a class inherits, and anything a `__getattr__` can
supply are all defined.

`m.pyi`:

```pyi
def f() -> None: ...
def lazy() -> None: ...

class Base:
    def m(self) -> None: ...

class A(Base):
    def m(self) -> None: ...

class B:
    def anything(self) -> None: ...

class Namespace:
    def __getattr__(self, name: str) -> int: ...
```

`_impl.py`:

```py
def f():
    pass
```

`m.py`:

```py
from typing import Any

from _impl import f

class Base:
    def m(self):
        pass

class A(Base):
    pass

class B:
    def __getattr__(self, name: str) -> Any:
        return None

def __getattr__(name: str) -> Any:
    return None

# a stub's `__getattr__` says only that the class has attributes the stub does not list
class Namespace: ...
```

## a public member the stub does not declare

`missing-stub-member` is disabled by default, since a stub often describes only the supported part
of a module.

`m.pyi`:

```pyi
def f() -> None: ...
```

`m.py`:

```py
import os

def f():
    pass

def g():  # error: [missing-stub-member] "`g` is public but not declared by the stub of this module"
    pass

def _helper():
    pass
```

## a parameter list the stub's calls do not fit

every call the stub accepts has to reach the runtime function.

`m.pyi`:

```pyi
def f(a: int, b: int = ..., *, c: int) -> None: ...
def g(a: int, /) -> None: ...
def h(a: int) -> None: ...
def k(a: int, *args: int, **kwargs: int) -> None: ...
def anything(a: int, b: int = ...) -> None: ...
```

`m.py`:

```py
# error: [incompatible-stub-signature] "`f` has no parameter for the stub's parameter `c`"
# error: [incompatible-stub-signature] "Parameter `b` of `f` has no default, but its stub lets a call leave it out"
# error: [incompatible-stub-signature] "Parameter `d` of `f` is required, but a call its stub accepts leaves it out"
def f(a, b, *, d):
    pass

def g(b):  # no diagnostic: a positional-only parameter may be named anything
    pass

def h(*, a):  # error: [incompatible-stub-signature] "Parameter `a` of `h` cannot be passed by position, as its stub declares"
    pass

def k(a, *args):  # error: [incompatible-stub-signature] "`k` has no parameter for the stub's parameter `kwargs`"
    pass

def anything(*args, **kwargs):  # no diagnostic
    pass
```

## an annotation that disagrees with the stub's

a parameter annotation has to accept what the stub says is passed, and a return annotation has to
promise no more than the stub's.

`m.pyi`:

```pyi
def f(a: int, b: int) -> int: ...
```

`m.py`:

```py
# error: [incompatible-stub-signature] "`f` returns `object`, which is not assignable to the stub's return type `int`"
def f(a: str, b: object) -> object:  # snapshot
    return 1
```

```snapshot
error[incompatible-stub-signature]: Parameter `a` of `f` is annotated `str`, which does not accept the stub's `int`
 --> src/m.py:2:10
  |
2 | def f(a: str, b: object) -> object:  # snapshot
  |          ^^^
  |
 ::: src/m.pyi:1:5
  |
1 | def f(a: int, b: int) -> int: ...
  |     ------------------------ the stub's declaration
```

## an overloaded function is held to every overload

`m.pyi`:

```pyi
from typing import overload

@overload
def f(a: int) -> int: ...
@overload
def f(a: str, b: str) -> str: ...
```

`m.py`:

```py
def f(a, b):  # error: [incompatible-stub-signature] "Parameter `b` of `f` is required, but a call its stub accepts leaves it out"
    return a
```

## the module's own overloads

where the module overloads the function itself, its implementation is what takes every call the stub
accepts. each of its `@overload`s declares a signature of its own.

`m.pyi`:

```pyi
from typing import overload

@overload
def f(a: int) -> int: ...
@overload
def f(a: str) -> str: ...
```

`m.py`:

```py
from typing import overload

@overload
def f(a: int) -> int: ...
@overload
def f(a: str) -> str: ...
def f(a: int | str) -> int | str:  # no diagnostic
    return a
```

## a variable annotated differently from the stub

`m.pyi`:

```pyi
x: int
y: int

class A:
    z: str
```

`m.py`:

```py
x: str = ""  # error: [incompatible-stub-attribute] "`x` is declared as `str` here but as `int` in the stub"
y: int = 1

class A:
    def __init__(self):
        self.z: bytes = b""  # error: [incompatible-stub-attribute] "`z` is declared as `bytes` here but as `str` in the stub"
```

## a class that does not inherit from the stub's bases

`m.pyi`:

```pyi
from typing import Protocol

class Base: ...
class E(ValueError): ...
class F(Base): ...
class G(Exception): ...
class P(Protocol): ...
class H(P): ...
```

`m.py`:

```py
class Base: ...

# error: [incompatible-stub-bases] "Class `E` does not inherit from `ValueError`, which its stub declares as a base"
class E(Exception): ...

# error: [incompatible-stub-bases] "Class `F` does not inherit from `Base`, which its stub declares as a base"
class F: ...
class G(ValueError): ...
class P: ...
class H: ...
```

a decorator may replace the class with another, whose bases the class statement does not show:

`n.pyi`:

```pyi
class E(ValueError): ...
```

`n.py`:

```py
from typing import Any

def _rebuild(cls: type) -> Any:
    return cls

@_rebuild
class E: ...  # no diagnostic
```
