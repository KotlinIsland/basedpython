# used underscore names

basedpython spells privacy with `private` and `protected`, so in a `.by` file a leading underscore
says only that a name is unused. a use of one is `used-underscore-name`

## a use of an underscore name

a function, a class, a variable and a parameter are all reported where they are read

```by
def _parse(text: str) -> int:
    return int(text)

class _Config: ...

_limit = 3

def scale(_factor: int) -> int:
    return _factor * 2  # error: [used-underscore-name] "`_factor` is used, but its leading underscore marks it unused"

_parse("1")  # error: [used-underscore-name] "`_parse` is used, but its leading underscore marks it unused"
_Config()  # error: [used-underscore-name]
print(_limit)  # error: [used-underscore-name]
```

## an unused underscore name

declaring an underscore name, and never reading it, is what the underscore says

```by
def handler(_event: str) -> None: ...

for _index in range(3):
    pass

_ignored = handler("click")
```

## `_` and dunders

`_` and a dunder have meanings python gives them, so neither is reported

```by
for _ in range(3):
    print(_)

def __helper__() -> int:
    return 1

__helper__()
```

## an augmented assignment reads its target

`_count += 1` reads `_count` before it writes it

```by
_count = 0
_count += 1  # error: [used-underscore-name]
```

## a name imported from another `.by` module

importing an underscore name reads it from the module that declared it, and so does every use

`helpers.by`:

```by
def _parse(text: str) -> int:
    return int(text)
```

`main.by`:

```by
import helpers
from helpers import _parse  # error: [used-underscore-name]

_parse("1")  # error: [used-underscore-name]
helpers._parse("1")  # error: [used-underscore-name]
```

## the report points at the declaration it resolved to

<!-- snapshot-diagnostics -->

a name declared in another file is the case where where it was spelled is most of the answer

`shared.by`:

```by
def _parse(text: str) -> int:
    return int(text)
```

`main.by`:

```by
from shared import _parse  # error: [used-underscore-name]

_parse("1")  # error: [used-underscore-name]
```

## an alias spells a name of its own

an import alias picks the spelling it binds. `as _local` is spelled here, so its uses are reported,
and `as parse` replaces the underscore name with one of its own — leaving only the import, which
reads `_parse` from the module that declared it

`tools.by`:

```by
def _parse(text: str) -> int:
    return int(text)

def parse(text: str) -> int:
    return int(text)
```

`main.by`:

```by
from tools import parse as _local
from tools import _parse as parse  # error: [used-underscore-name]

_local("1")  # error: [used-underscore-name]
parse("1")
```

## python and stubs spell their own names

a name python or a stub declares is not the `.by` author's to rename

`library.py`:

```py
def _helper() -> int:
    return 1

class Base:
    _state: int = 0

    def _hook(self) -> None: ...
```

`main.by`:

```by
import sys
from library import _helper, Base

_helper()
sys._getframe()
print(Base()._state)
```

## an override keeps the name of the member it overrides

a member declared on a python base class keeps its python name, even where a `.by` class overrides
it

`framework.py`:

```py
class Handler:
    def _hook(self) -> None: ...
```

`main.by`:

```by
from framework import Handler

class Mine(Handler):
    def _hook(self) -> None: ...

    def run(self) -> None:
        self._hook()
```

## a member declared in a `.by` class

a member of a `.by` class is read through `self`, through an instance, and through the class itself

```by
class Account:
    _rate: float = 0.05

    def __init__(self) -> None:
        self._balance = 0

    def interest(self) -> float:
        # error: [used-underscore-name] "`_balance` is used"
        # error: [used-underscore-name] "`_rate` is used"
        return self._balance * self._rate

def audit(account: Account) -> int:
    return account._balance  # error: [used-underscore-name]

print(Account._rate)  # error: [used-underscore-name]
```

## a member of a stub class

a `NamedTuple`'s `_asdict` is declared by typeshed

```by
from typing import NamedTuple

class Point(NamedTuple):
    x: int

Point(1)._asdict()
```

## a class pattern reads its keywords

a keyword in a class pattern names a member of the subject, so it reads one exactly as `box._item`
does. the assignment that declares the member is not a use

```by
class Box:
    def __init__(self, _item: int) -> None:
        self._item = _item  # error: [used-underscore-name] "`_item` is used"

def unbox(box: Box) -> int:
    match box:
        case Box(_item=item):  # error: [used-underscore-name]
            return item
    return 0
```

the `_item` reported above is the parameter being read, not the attribute being written: a write is
not a use, and neither is a declaration

```by
class Crate:
    _item: int = 0

    def fill(self) -> None:
        self._item = 1
```

## an `extension` member

a member an `extension` supplies is not declared by the class it extends, but it is still a name a
`.by` file spelled

```by
extension list:
    def _second(self) -> object:
        return self[1]

def pick(xs: list[int]) -> object:
    return xs._second()  # error: [used-underscore-name]
```

an extension body reads the members of the class it extends, and those are the extended class's own
names

```by
class Stack[T]:
    def __init__(self, items: list[T]) -> None:
        self._items = items

extension Stack:
    def peek(self) -> T:
        return self._items[-1]  # error: [used-underscore-name]
```

## a receiver callable

a callable with a receiver is read off the receiver like a member, and is still the name the
parameter declared

```by
def apply(_render: int.() -> str) -> str:
    return (1)._render()  # error: [used-underscore-name]
```

## a member of an inline protocol

an inline protocol declares its members in the annotation the receiver was written with, and that is
where the name was spelled

```by
def read(p: protocol(_size: int)) -> int:
    return p._size  # error: [used-underscore-name]
```

## a member read unqualified in a trailing lambda block

a trailing lambda block sees its receiver's members unqualified, so a bare name in one reads a
member

```by
class Builder:
    _size: int = 0

def build(block: Builder.() -> None) -> None: ...

build():
    print(_size)  # error: [used-underscore-name]
```

## a keyword argument names a parameter

a caller writing `f(_p=1)` names the parameter, so the underscore contradicts the signature at the
call site. a dataclass field is the same: its name is the keyword its constructor takes

```by
from dataclasses import dataclass

def scale(_factor: int) -> int:
    return _factor * 2  # error: [used-underscore-name]

@dataclass
class Point:
    _x: int

scale(_factor=2)  # error: [used-underscore-name]
Point(_x=1)  # error: [used-underscore-name]
```

## `del` names the binding it deletes

```by
_cache = {1: 2}
del _cache  # error: [used-underscore-name]
```

## `super()` reads the member off the base it resolves to

```by
class Base:
    def _hook(self) -> int:
        return 1

class Child(Base):
    def run(self) -> int:
        return super()._hook()  # error: [used-underscore-name]
```

## an `init(...)` parameter declares an attribute

`init(let _x: int)` stands for a `self._x = _x` the author never wrote. the attribute it declares is
spelled by the parameter, so reading it is reported — and the assignment standing behind the
parameter is not a read of anything

```by
class Account:
    init(self, let _balance: int)

def audit(account: Account) -> int:
    return account._balance  # error: [used-underscore-name]
```

## a submodule of a `.by` package

each segment of an import names a module, and a module is spelled by the name of its file

`pkg/__init__.by`:

```by
```

`pkg/_internal.by`:

```by
def helper() -> int:
    return 1
```

`main.by`:

```by
import pkg._internal  # error: [used-underscore-name]

pkg._internal.helper()  # error: [used-underscore-name]
```

## a class that mixes `.by` and python declarations

where the classes a receiver can be disagree about who spelled the name, the python spelling decides
it: renaming would break that half

`library.py`:

```py
class Vendor:
    _state: int = 0
```

`main.by`:

```by
from library import Vendor

class Mine:
    _state: int = 0

def read(x: Mine | Vendor) -> int:
    return x._state
```

## a member declared in a stub

a `.byi` stub describes an interface someone else spelled

`vendor.byi`:

```byi
class Client:
    _session: int
```

`main.by`:

```by
from vendor import Client

def read(c: Client) -> int:
    return c._session
```

## a class with a base nothing can resolve

an unknown base may declare any member at all, so nothing here can be said to be the author's

```by
from unresolvable import Mystery  # error: [unresolved-import]

class Mine(Mystery):
    _x: int = 0

def read(m: Mine) -> int:
    return m._x
```

## names basedpython spells

a property's `field` reads storage the parser names `__x`, and an enum variant's anonymous fields
are `_0`, `_1`, … — the author wrote neither name, so neither is reported

```by
class Person:
    var age: int = 0
        get() = field
        set(value):
            field = value

enum class Shape:
    case Square(int)

print(Shape.Square(1)._0)
```

## a `.py` file keeps python's convention

in python a leading underscore means private, so nothing is reported

```py
def _parse(text: str) -> int:
    return int(text)

_parse("1")
```
