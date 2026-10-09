# unused imports

`unused-import` reports an import in a `.by` file that nothing uses. an import is used when a name
it binds is read, and also when the module it names declares an `extension` or a conformance that
supplied something the file relies on — the extension's members are only applicable in files that
import its module, so the import is what makes them reachable even when its name is never read.

the mdtest corpus turns this rule off, so the tests below turn it back on.

```toml
[environment]
python-version = "3.12"

[rules]
unused-import = "warn"
```

## an import nothing reads is reported

```by
import json  # error: [unused-import] "`json` imported but unused"
from os import path  # error: [unused-import] "`os.path` imported but unused"
from collections import OrderedDict, deque  # error: [unused-import] "`collections.deque` imported but unused"

OrderedDict()
```

## an import whose name is read is not

a read anywhere in the file counts, including from a nested function and from an annotation

```by
import json
from collections import OrderedDict

def dump(value: OrderedDict[str, int]) -> str:
    return json.dumps(value)
```

## an import used only through an extension

`colours` declares an extension of `Enum`, and `main` reads its member without ever reading the name
`colours`

`colours.by`:

```by
from enum import Enum

extension Enum:
    let lower_name: str
        get() = self.name.lower()
```

`main.by`:

```by
from enum import Enum
import colours

class Colour(Enum):
    RED = 1

def describe(colour: Colour) -> str:
    return colour.lower_name

describe(Colour.RED)
```

`colours` itself reads `Enum` only in the `extension Enum:` header, which names the type it extends

## an extension module whose extension nothing uses is reported

`colours.by`:

```by
from enum import Enum

extension Enum:
    let lower_name: str
        get() = self.name.lower()
```

`main.by`:

```by
import colours  # error: [unused-import]
```

## an import used only through a conformance

`render` asks for a `Show`, and `"hi"` is one only because `conforming` declares that `str`
conforms. nothing in `main` names `conforming`

`shows.by`:

```by
protocol Show:
    def show(self) -> str

def render(value: Show) -> str:
    return value.show()
```

`conforming.by`:

```by
from shows import Show

extension str(Show):
    override def show(self) -> str:
        return self
```

`main.by`:

```by
from shows import render
import conforming

render("hi")
```

## an import used only through a protocol extension reached by conformance

`loud` extends the protocol itself, and a conforming `str` reaches the member through the
conformance — so both imports are used, and neither name is read

`shows.by`:

```by
protocol Show:
    def show(self) -> str
```

`loud.by`:

```by
from shows import Show

extension Show:
    def shout(self) -> str:
        return self.show().upper()
```

`conforming.by`:

```by
from shows import Show

extension str(Show):
    override def show(self) -> str:
        return self
```

`main.by`:

```by
import loud
import conforming

def greet() -> str:
    return "hi".shout()
```

## an import used only for a default a conformance takes from a protocol extension

`str` answers `Show`'s requirement with the default `defaults` declares on the protocol, so the
conformance's witness table takes that default, and the import of `defaults` is what makes it
applicable

`shows.by`:

```by
protocol Show:
    def show(self) -> str
```

`defaults.by`:

```by
from shows import Show

extension Show:
    def show(self) -> str:
        return "?"
```

`conforming.by`:

```by
from shows import Show
import defaults

extension str(Show): ...
```

## an import that brings a second conformance of the same pair into view

the conflict is reported where the second conformance is declared, and the import that brings the
first into view is what causes it — so it is used until one of the two goes

`shows.by`:

```by
protocol Show:
    def show(self) -> str
```

`first.by`:

```by
from shows import Show

extension str(Show):
    override def show(self) -> str:
        return self
```

`second.by`:

```by
from shows import Show
import first

# error: [invalid-conformance]
extension str(Show):
    override def show(self) -> str:
        return self
```

## an import used only by a type test answered from the conformance registry

`value is Show` asks the registry, because a conformance to `Show` is in scope. without the import
the test is a plain `isinstance` against a protocol that does not allow one

`shows.by`:

```by
protocol Show:
    def show(self) -> str
```

`conforming.by`:

```by
from shows import Show

extension str(Show):
    override def show(self) -> str:
        return self
```

`main.by`:

```by
from shows import Show
import conforming

def check(value: object) -> bool:
    return value is Show
```

## an import used only through an operator an extension supplies

`money.by`:

```by
class Money:
    def __init__(self, cents: int) -> None:
        self.cents = cents
```

`arithmetic.by`:

```by
from money import Money

extension Money:
    def __add__(self, other: Money) -> Money:
        return Money(self.cents + other.cents)
```

`main.by`:

```by
from money import Money
import arithmetic

def total(a: Money, b: Money) -> Money:
    return a + b
```

## an import used only through a unary operator or a comparison an extension supplies

`money.by`:

```by
class Money:
    def __init__(self, cents: int) -> None:
        self.cents = cents
```

`negation.by`:

```by
from money import Money

extension Money:
    def __neg__(self) -> Money:
        return Money(-self.cents)
```

`ordering.by`:

```by
from money import Money

extension Money:
    def __lt__(self, other: Money) -> bool:
        return self.cents < other.cents
```

`main.by`:

```by
from money import Money
import negation
import ordering

def refund(a: Money) -> Money:
    return -a

def cheaper(a: Money, b: Money) -> bool:
    return a < b
```

## an import used only through a conversion an extension supplies

`ids.by`:

```by
class UserId:
    pass

def register(user: UserId) -> None: ...
```

`parsing.by`:

```by
from ids import UserId

extension UserId:
    class def __from__(cls, value: int) -> UserId:
        return UserId()
```

`main.by`:

```by
from ids import register
import parsing

register(3)
```

## every element of a collection literal that converts uses its extension

each element of the list converts through a different module's extension, so both imports are used —
the second element's as much as the first's

`ids.by`:

```by
class UserId:
    pass

class Tag:
    pass
```

`numbers.by`:

```by
from ids import UserId

extension UserId:
    class def __from__(cls, value: int) -> UserId:
        return UserId()
```

`labels.by`:

```by
from ids import Tag

extension Tag:
    class def __from__(cls, value: str) -> Tag:
        return Tag()
```

`main.by`:

```by
from ids import UserId, Tag
import numbers
import labels

xs: list[UserId | Tag] = [1, "s"]
```

## every element of a collection literal assigned to an attribute uses its extension

`ids.by`:

```by
class UserId:
    pass

class Tag:
    pass
```

`numbers.by`:

```by
from ids import UserId

extension UserId:
    class def __from__(cls, value: int) -> UserId:
        return UserId()
```

`labels.by`:

```by
from ids import Tag

extension Tag:
    class def __from__(cls, value: str) -> Tag:
        return Tag()
```

`main.by`:

```by
from ids import UserId, Tag
import numbers
import labels

class Holder:
    def __init__(self) -> None:
        self.xs: list[UserId | Tag] = []

holder = Holder()
holder.xs = [1, "s"]
```

## any import of the module grants its extensions

a `from` import of a name the file never reads still makes the module's extensions applicable, so it
is used when one of them is

`colours.by`:

```by
from enum import Enum

extension Enum:
    let lower_name: str
        get() = self.name.lower()

def unrelated() -> None: ...
```

`main.by`:

```by
from enum import Enum
from colours import unrelated

class Colour(Enum):
    RED = 1

def describe(colour: Colour) -> str:
    return colour.lower_name
```

## an import inside a function grants the whole file

an import makes a module's extensions applicable wherever in the file it is written, so one inside a
function that is never called is still used by an extension member read anywhere else

`colours.by`:

```by
from enum import Enum

extension Enum:
    let lower_name: str
        get() = self.name.lower()
```

`main.by`:

```by
from enum import Enum

class Colour(Enum):
    RED = 1

def setup() -> None:
    import colours

def describe(colour: Colour) -> str:
    return colour.lower_name
```

## an extension member read in a receiver block

a bare name inside a trailing lambda block whose callback declares a receiver reads the receiver's
members, and an extension's members with them

`tags.by`:

```by
class Tag:
    def __init__(self) -> None:
        self.children: list[str] = []
```

`paragraphs.by`:

```by
from tags import Tag

extension Tag:
    def p(self, text: str) -> None:
        self.children.append(text)
```

`main.by`:

```by
from tags import Tag
import paragraphs

def page(fn: Tag.() -> None) -> Tag:
    tag = Tag()
    fn(tag)
    return tag

page:
    p("hello")
```

## a receiver callable read as a method

`x.fn()` reads `fn`, a receiver callable in scope, when `x` has no member of that name

`render.by`:

```by
def describe(value: int) -> str:
    return str(value)

renderer: int.() -> str = describe
```

`main.by`:

```by
from render import renderer

def show(x: int) -> str:
    return x.renderer()
```

## an import read only as a context-sensitive name's qualifier

`Red` where a `Color` is expected is lowered to `Color.Red`, so the import of `Color` is read
although the source never spells the name

`palette.by`:

```by
enum class Color:
    case Red, Green

def paint(c: Color) -> str:
    return str(c)
```

`main.by`:

```by
from palette import Color, paint

paint(Red)
```

## an import read only as a case name's qualifier

`case Red:` is lowered to `case Color.Red:`, and `case Circle(r):` to `case Shape.Circle(r):`

`palette.by`:

```by
enum class Color:
    case Red, Green

enum class Shape:
    case Circle(radius: float)
    case Dot

def current() -> Color:
    return Color.Green

def outline() -> Shape:
    return Shape.Dot
```

`main.by`:

```by
from palette import Color, Shape, current, outline

def describe() -> str:
    match current():
        case Red:
            return "red"
        case _:
            return "other"

def radius() -> float:
    match outline():
        case Circle(r):
            return r
        case _:
            return 0.0
```

## an import read only by a type test's runtime check

`x is AB` is lowered to a test of each arm of the alias, `isinstance(x, A) or isinstance(x, B)`, so
it reads `A` and `B` although the source spells neither

`kinds.by`:

```by
class A:
    pass

class B:
    pass

type AB = A | B
```

`main.by`:

```by
from kinds import AB, A, B

def check(x: object) -> bool:
    return x is AB
```

## an import read only by a type test against a module attribute

`kinds.by`:

```by
class A:
    pass

class B:
    pass

type AB = A | B
```

`main.by`:

```by
import kinds
from kinds import A, B

def check(x: object) -> bool:
    return x is kinds.AB
```

## an import read only by a checked cast's runtime check

`kinds.by`:

```by
class A:
    pass

class B:
    pass

type AB = A | B
```

`main.by`:

```by
from kinds import AB, A, B

def narrow(x: object) -> AB:
    return x cast! AB
```

## an import read only by a `raises` guard

the guard a `raises` function is lowered behind names the exceptions it may raise

`errors.by`:

```by
class Oops(Exception):
    pass

class Bad(Exception):
    pass

type Errs = Oops | Bad
```

`main.by`:

```by
from errors import Errs, Oops, Bad

def run() -> None raises Errs:
    raise Oops()
```

## explicit re-exports are not reported

`import a as a`, `from a import b as b` and basedpython's `from a export b` say the name is part of
this module's interface

```by
import json as json
from os import path as path
from collections export OrderedDict
```

## a name listed in `__all__` is exported

```by
import json
from os import path

__all__ = ["json", "path"]
```

## a name listed in an `__all__` the checker cannot read in full

`__all__` built by concatenating something other than a list of names is too dynamic for the checker
to know every name in it, but a name spelled out in it is still exported

```by
import json

extra: list[str] = []
__all__ = ["json"] + extra
```

## a file that does not parse is not checked

a read the parser could not recover is invisible, so an import in a file with a syntax error is
never reported

```by
import json

x = 1 +  # error: [invalid-syntax]
```

## a name another module's `__all__` adds to this one's

the checker follows `__all__ += helpers.__all__` to the names `helpers` lists, so a name imported
from it is exported

`helpers.by`:

```by
def helper() -> None: ...

__all__ = ["helper"]
```

`main.by`:

```by
import helpers
from helpers import helper

__all__ = ["helpers"]
__all__ += helpers.__all__
```

## a `__future__` import is a directive

```by
from __future__ import annotations
```

## an import rebound by something other than an import is not reported

an assignment that rebinds the name makes the import a redefinition rather than an unused import,
which is `F811`'s concern

```by
import json
import json as codec

json = codec
```

## a read of an earlier binding counts for a later one

```by
import os

os.getcwd()

import os
```

## every one of several unread imports of a name is reported

an import rebound by another import before anything reads it is reported too, so one round of fixes
removes them all

```by
import json  # error: [unused-import]
import json  # error: [unused-import]
```

## a `del` starts the name afresh

a read before the `del` belongs to the binding it deletes, so it does not use an import made after

```by
import os

os.getcwd()
del os

import os  # error: [unused-import]
```

## a deleted import is not reported

`del` reads the binding it deletes, and leaves the name unbound

```by
import os

del os
```

## a name declared `global` binds in the module

an import in a function that declares the name `global` binds the module's name, and is not reported
there. the module's own import is reported when nothing reads the name

```by
import json  # error: [unused-import]

def reload() -> None:
    global json
    import json
```

## a read through a `global` declaration uses the module's import

```by
import json

def dump() -> str:
    global json
    return json.dumps(1)
```

## an import in a class body is not reported

```by
class Config:
    import json
```

## a submodule import binds the top-level package

`import os.path` binds `os`, so a read of `os` uses it

```by
import os.path

def cwd() -> str:
    return os.getcwd()
```

## an unread submodule import is reported by its full name

```by
import os.path  # error: [unused-import] "`os.path` imported but unused"
```

## two submodule imports bind one name

`import os.path` and `import email.mime` bind `os` and `email`; a second submodule of the same
package rebinds the same name, so a read through either uses both

```by
import email.mime
import email.parser

def parser() -> object:
    return email.parser
```

## an import of the package does not redefine a submodule import

`import os` after `import os.path` binds `os` again, but `os.path` stays reachable through it only
because of the first import, so the first is not an unused redefinition

```by
import os.path
import os

os.getcwd()
```

## a read earlier in a loop body uses an import later in it

the second time round the loop, the read sees the import the first pass made. the checker cannot
tell that the first pass always takes the other branch, so it reports the read too

```by
def poll() -> None:
    for attempt in range(2):
        if attempt:
            os.getcwd()  # error: [possibly-unresolved-reference]
        else:
            import os
```

## a name in a `raises` clause is read

```by
from json import JSONDecodeError

def parse(text: str) -> int raises JSONDecodeError:
    raise JSONDecodeError("bad", text, 0)
```

## an import read in an annotation in a `TYPE_CHECKING` block

```by
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections import OrderedDict

def first(value: OrderedDict[str, int]) -> int:
    return 0
```

## an unread import in a `TYPE_CHECKING` block is reported

```by
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections import OrderedDict  # error: [unused-import]
```

## an annotation may read an import written after it

annotations are evaluated lazily, so a name in one resolves to every binding the scope makes

```by
def first(value: OrderedDict[str, int]) -> int:
    return 0

from collections import OrderedDict
```

## a string in a type position reads no name

in a `.by` file a string in a type position is a literal type, never a forward reference, so it uses
nothing

```by
from collections import OrderedDict  # error: [unused-import]

def name(value: "OrderedDict") -> None: ...
```

## an import that tests for a module's availability

an import in the body of a `try` that handles `ImportError` is usually there to find out whether the
module is installed. it is still reported, and says how to ask without importing

```by
try:
    import tomllib  # snapshot
except ImportError:
    pass
```

```snapshot
warning[unused-import]: `tomllib` imported but unused
 --> src/mdtest_snippet.by:2:12
  |
2 |     import tomllib  # snapshot
  |            ^^^^^^^
help: To test whether a module is available, use `importlib.util.find_spec` instead of importing it
```

## an import in a function defined inside such a `try` is not a test

the function's body runs when it is called, outside the `try`, so its import gets the ordinary fix

```by
try:
    import tomllib  # error: [unused-import]

    def load() -> None:
        import json  # snapshot
except ImportError:
    pass
```

```snapshot
warning[unused-import]: `json` imported but unused
 --> src/mdtest_snippet.by:5:16
  |
5 |         import json  # snapshot
  |                ^^^^
help: Remove the unused import `json`
  |
4 |     def load() -> None:
  -         import json  # snapshot
5 +         pass  # snapshot
6 | except ImportError:
  |
```

## an unused import in a package's `__init__` may be a re-export

a package's `__init__.by` is what importing the package runs, so an import there may be how the
package exports the name. it is still reported, and one of the project's own modules says how to
make the re-export explicit

`pkg/models.by`:

```by
class Widget:
    pass
```

`pkg/__init__.by`:

```by
import json  # error: [unused-import]
from .models import Widget  # snapshot
```

```snapshot
warning[unused-import]: `.models.Widget` imported but unused
 --> src/pkg/__init__.by:2:21
  |
2 | from .models import Widget  # snapshot
  |                     ^^^^^^
help: If this is a re-export, write it as `from … export …`, as a redundant alias, or list it in `__all__`
```

## an import in a function is reported in its scope

```by
def load() -> None:
    import json  # error: [unused-import]
```

## the fix removes an unused alias and keeps the rest of the statement

```by
from collections import OrderedDict, deque  # snapshot

OrderedDict()
```

```snapshot
warning[unused-import]: `collections.deque` imported but unused
 --> src/mdtest_snippet.by:1:38
  |
1 | from collections import OrderedDict, deque  # snapshot
  |                                      ^^^^^
help: Remove the unused import `collections.deque`
  |
  - from collections import OrderedDict, deque  # snapshot
1 + from collections import OrderedDict  # snapshot
2 |
  |
```

## the fix removes an alias with its own line inside parentheses

```by
from collections import (
    OrderedDict,  # read below
    deque,  # snapshot
)

OrderedDict()
```

```snapshot
warning[unused-import]: `collections.deque` imported but unused
 --> src/mdtest_snippet.by:3:5
  |
3 |     deque,  # snapshot
  |     ^^^^^
help: Remove the unused import `collections.deque`
  |
2 |     OrderedDict,  # read below
  -     deque,  # snapshot
3 | )
  |
```

## the fix writes `pass` where removing the import would leave a block empty

```by
def load() -> None:
    import json  # snapshot
```

```snapshot
warning[unused-import]: `json` imported but unused
 --> src/mdtest_snippet.by:2:12
  |
2 |     import json  # snapshot
  |            ^^^^
help: Remove the unused import `json`
  |
1 | def load() -> None:
  -     import json  # snapshot
2 +     pass  # snapshot
  |
```
