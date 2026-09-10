# basedpython: visibility modifiers

`export`/`public` add a module-level symbol to the generated `__all__`. `private` and `protected`
say who may reach a member: `private` only the declaring class's own body, `protected` a subclass's
body as well. neither carries a type-level effect — the declaration keeps its ordinary type rather
than being erased to `Unknown`.

the lowering spells the answer in the member's name, because that is the only enforcement the
runtime offers: `private` renames to `__name`, which python name-mangles per class, and `protected`
to `_name`, python's own convention for "not part of the interface". the type checker enforces both
directly, so neither spelling has to be relied on.

a dunder is the exception: python looks one up by its exact name, and mangles only names with at
most one trailing underscore, so renaming would change what the method *is* rather than who can
reach it. a visibility keyword on one is therefore reported as having no effect — except on
`__init__`, the one dunder where it says something, which is checked at the construction site
instead.

## a private class keeps its type

```by
private class Box:
    value: int = 0

b = Box()
reveal_type(b)  # revealed: final Box
reveal_type(b.value)  # revealed: int
```

## an exported function keeps its signature

```by
export def make(n: int) -> int:
    return n * 2

reveal_type(make)  # revealed: def make(n: int) -> int
reveal_type(make(3))  # revealed: int
```

## `open` is a no-op modifier on the type

`open` marks a class freely subclassable (the default in Python); it only suppresses the closed-by-
default checks, so the class type is unchanged.

```by
open class Base:
    tag: str = "b"

reveal_type(Base().tag)  # revealed: str
```

## modifiers compose with each other

a chain of modifiers still resolves to the underlying declaration's type.

```by
private final class Sealed:
    n: int = 1

reveal_type(Sealed().n)  # revealed: int
```

## `private` on a name it cannot hide

`private` hides a member by renaming it so python's name-mangling applies. a name python looks up
verbatim is left as written, so the modifier would silently do nothing, and it is reported instead.

```by
class Point:
    private def __repr__(self) -> str:  # error: [ineffective-private]
        return "Point()"

    # `__` + `_` is `___`, which python also looks up verbatim
    private def _(self): ...  # error: [ineffective-private]
```

a name with at most one trailing underscore is hidden as usual, and `__init__` is the one dunder
`private` does say something about.

```by
class Id:
    private init()

    private def helper(self): ...
    private def unhide_(self): ...
```

## a `private` constructor may only be called by its own class

name-mangling is how `private` hides a class member, but python calls a constructor by its exact
name, so there is no spelling that would hide `__init__` and still leave it a constructor. a
`private init` is enforced at the construction site instead: the class's own body may construct it,
and nothing else may. that is what lets a class hand out its instances through a factory of its own.

```by
class Id:
    private init(let raw: str)

    @classmethod
    def parse(cls, text: str) -> Id:
        return Id(text.strip())

reveal_type(Id.parse(" a ").raw)  # revealed: str

made = Id("a")  # error: [private-constructor]
```

## the diagnostic points back at the declaration

the class that drew the boundary is named at the construction site and annotated where it declared
the constructor.

```by
class Id:
    private init(let raw: str)

made = Id("a")  # snapshot
```

```snapshot
error[private-constructor]: Cannot construct `Id`: its constructor is private
 --> src/mdtest_snippet.by:4:8
  |
4 | made = Id("a")  # snapshot
  |        ^^^^^^^
info: Only code inside `Id` may construct it
 --> src/mdtest_snippet.by:2:13
  |
2 |     private init(let raw: str)
  |             ---- `Id`'s constructor declared private here
```

## every scope inside the class is the class's own code

a method's local function and a nested class are written inside the body, so they construct it too.

```by
class A:
    private init()

    def clone(self):
        def build():
            return A()

        return build()

    class Inner:
        @staticmethod
        def make():
            return A()

reveal_type(A.Inner.make().clone())  # revealed: final A
```

## a subclass is outside a `private` constructor's class

a subclass may not construct its base, and — because it inherits the private constructor — may not
be constructed itself.

```by
class Base:
    private init()

class Derived(Base): ...

base = Base()  # error: [private-constructor]
derived = Derived()  # error: [private-constructor]
```

## the boundary is the declaring class, not the declaring module

another module may hold the class, name it, and pass it around. what it may not do is call it.

`ids.by`:

```by
class Id:
    private init(let raw: str)

    @classmethod
    def parse(cls, text: str) -> Id:
        return Id(text.strip())
```

`callers.by`:

```by
from ids import Id

parsed = Id.parse(" a ")
reveal_type(parsed.raw)  # revealed: str

made = Id("a")  # error: [private-constructor]

# naming the class does not launder it: the value is still the class itself
alias = Id
aliased = alias("a")  # error: [private-constructor]
```

## a subclass reusing its base's name is still told whose constructor it is

the two classes are told apart by identity rather than by name, so the message says the constructor
was inherited even where both classes are called `Id`.

`base_id.by`:

```by
class Id:
    private init()
```

`shadowing.by`:

```by
import base_id

class Id(base_id.Id): ...

# error: [private-constructor] "Cannot construct `Id`: it inherits `Id`'s private constructor"
made = Id()
```

## `type[A]` is not refused

a `type[A]` may hold a subclass, and a subclass is free to declare a constructor of its own — the
same reason `type[SomeProtocol]` may be called where the protocol class itself may not. so the
guarantee a `private init` gives is over the class's own name, not over every route to a class
object.

```by
class Id:
    private init()

def build(cls: type[Id]) -> Id:
    return cls()
```

## declaring a constructor makes a subclass constructible again

```by
class Base:
    private init()

class Derived(Base):
    init()

derived = Derived()
reveal_type(derived)  # revealed: final Derived
```

## `private type` aliases bind the unmangled name

the `_` prefix is applied by the lowering; in the type checker the alias binds the name as written.

```by
private type Key = str | int

def lookup(k: Key) -> None: ...

reveal_type(lookup)  # revealed: def lookup(k: Key)
```

## a private alias may be used freely inside its own module

`store.by`:

```by
private type Key = str | int

type Table = dict[Key, int]

def get(t: Table, k: Key) -> int:
    return t[k]
```

## importing a private symbol from another module is an error

`helpers.by`:

```by
private type Key = str | int

private def secret() -> int:
    return 1

private class Hidden: ...

type Open = list[int]
```

`main.by`:

```by
from helpers import Open  # fine
from helpers import Key  # error: [private-import] "`Key` is private to `helpers`"
from helpers import secret  # error: [private-import] "`secret` is private to `helpers`"
from helpers import Hidden  # error: [private-import] "`Hidden` is private to `helpers`"
```

## renaming on import does not launder a private symbol

`helpers2.by`:

```by
private type Key = str | int
```

`main2.by`:

```by
from helpers2 import Key as K  # error: [private-import] "`Key` is private to `helpers2`"
```

## a real `@private` decorator is not the modifier

a decorator written with `@` is an ordinary decorator, so the symbol stays importable.

`deco.by`:

```by
def private[T](f: T) -> T:
    return f

@private
def helper() -> int:
    return 1
```

`main3.by`:

```by
from deco import helper

reveal_type(helper())  # revealed: int
```

## a private member is reachable from its own class

the class's own body reaches a private member by the name it was written with; the lowering is what
spells out the mangled one.

```by
class Account:
    private balance: int = 0

    init(private let owner: str)

    private def audit(self) -> str:
        return f"{self.owner}:{self.balance}"

    def report(self) -> str:
        return self.audit()

reveal_type(Account("a").report())  # revealed: str
```

## a private member is not reachable from a subclass

python mangles `__name` with the name of the class whose body it is written in, so a subclass's body
names a different attribute entirely. that is what `private` means, and it is refused rather than
silently renamed to reach across.

```by
class Base:
    private secret: int = 1

class Derived(Base):
    def leak(self) -> int:
        return self.secret  # error: [inaccessible-member]
```

## a private member is not reachable from outside any class

```by
class Base:
    private secret: int = 1

def read(b: Base) -> int:
    return b.secret  # error: [inaccessible-member]
```

## a protected member is reachable from a subclass

`protected` is the visibility `private` is usually mistaken for: the declaring class and everything
that inherits from it.

```by
class Base:
    protected step: int = 2

    init(protected let limit: int)

class Derived(Base):
    def describe(self) -> str:
        return f"{self.step}/{self.limit}"

reveal_type(Derived(4).describe())  # revealed: str
```

## a protected member is not reachable from outside the hierarchy

```by
class Base:
    protected step: int = 2

def read(b: Base) -> int:
    return b.step  # error: [inaccessible-member]
```

## a member is not reachable through a write either

the boundary is about the member, not about which direction it is used in.

```by
class Base:
    private count: int = 0

def bump(b: Base) -> None:
    b.count = 1  # error: [inaccessible-member]
```

## a plain underscore name is left to convention

`_name` and `__name` written out are what python itself offers, and they mean whatever the author
meant by them. only a visibility keyword makes the boundary something to enforce.

```by
class Base:
    _step: int = 2

def read(b: Base) -> int:
    return b._step
```

## declaring a member less visible than the one it inherits

a visibility keyword decides the name the member is emitted under, so a member declared less visible
than the one it inherits does not override it — it sits beside it under a different name, and the
inherited one still answers.

```by
class A:
    def f(self) -> int:
        return 1

class B(A):
    # error: [invalid-override-visibility]
    private def f(self) -> int:
        return 2
```

## the narrowing is reported whether or not `override` is written

writing `override` as well states the opposite of what `private` does, but the declaration is wrong
either way, so the report does not depend on it.

```by
class A:
    def f(self) -> int:
        return 1

class B(A):
    # error: [invalid-override-visibility]
    private override def f(self) -> int:
        return 2
```

## `protected` over `public` narrows too

```by
class A:
    def f(self) -> int:
        return 1

class B(A):
    # error: [invalid-override-visibility]
    protected def f(self) -> int:
        return 2
```

## widening is not narrowing

a subclass is free to declare a member of its own under a name a base kept private: the two are
different attributes, and the public one is new rather than a replacement.

```by
class A:
    private def f(self) -> int:
        return 1

class B(A):
    def f(self) -> int:
        return 2

reveal_type(B().f())  # revealed: int
```

## two private members of the same name are unrelated

each is mangled with the name of the class that declares it, so neither overrides the other and
neither has to match the other's signature.

```by
class A:
    private def helper(self, n: int) -> int:
        return n

class B(A):
    private def helper(self, s: str) -> str:
        return s

    def use(self) -> str:
        return self.helper("x")

reveal_type(B().use())  # revealed: str
```

## a protected member overrides like any other

`protected` keeps one name across the hierarchy, so a subclass's declaration really does replace the
one it inherits, and has to be substitutable for it.

```by
class A:
    protected def f(self, n: int) -> int:
        return n

class B(A):
    # error: [invalid-method-override]
    protected override def f(self, n: str) -> int:
        return len(n)
```

## `protected` is only a modifier on a class member

outside a class body there is nothing for the "and its subclasses" half of `protected` to mean.

```by
protected def helper() -> int:  # error: [invalid-syntax]
    return 1
```

## a protected constructor may be called by a subclass

`private init` says the class decides how its instances are made. `protected init` extends that to
the classes that inherit it, which is what a base class does when only its subclasses should
construct it — a subclass's body may construct itself, or the base.

```by
class Shape:
    protected init(let sides: int)

class Square(Shape):
    @classmethod
    def make(cls) -> Square:
        return Square(4)

    @classmethod
    def base(cls) -> Shape:
        return Shape(4)

reveal_type(Square.make().sides)  # revealed: int
reveal_type(Square.base().sides)  # revealed: int
```

## a protected constructor is still refused outside the hierarchy

```by
class Shape:
    protected init(let sides: int)

Shape(3)  # error: [private-constructor]
```

## a visibility keyword composes with a class variable

`class var`, `class let` and `class x = v` take no other modifier, since their own keyword fills the
one slot a declaration has. A visibility keyword is the exception: it says who may reach the
variable, which composes with any declaration, and the class reaches it through the class object as
readily as through an instance.

```by
class Counter:
    private class var made: int = 0
    protected class let LIMIT: int = 3
    private class hits = 0

    @classmethod
    def total(cls) -> int:
        return cls.made + cls.LIMIT + cls.hits

reveal_type(Counter.total())  # revealed: int
```

## a private class variable is not reachable through the class from outside

```by
class Counter:
    private class var made: int = 0

Counter.made  # error: [inaccessible-member]
```

## a module-level private variable cannot be imported

A variable is renamed like a function, class or type alias is, so it is taken off the module's
interface the same way.

`helpers.by`:

```by
private count: int = 0
private total = 0
```

`main.by`:

```by
from helpers import count  # error: [private-import] "`count` is private to `helpers`"
from helpers import total  # error: [private-import] "`total` is private to `helpers`"
```

## a module's private symbol is not reachable as an attribute of the module

Reaching the symbol through the module object crosses the same boundary an import does, and the
lowering has renamed it, so the attribute is not there at runtime either.

`helpers.by`:

```by
private count: int = 0

private def secret() -> int:
    return 1
```

`main.by`:

```by
import helpers

# error: [inaccessible-member] "`count` is private to module `helpers`"
helpers.count
# error: [inaccessible-member] "`secret` is private to module `helpers`"
helpers.secret()
```

## a module's own code reaches its private variable

```by
private count: int = 0

def bump() -> int:
    global count
    count += 1
    return count

reveal_type(bump())  # revealed: int
```

## a bare name in the class body reaches a restricted member

The class body names its own members without a receiver, and the lowering renames those names along
with the declarations.

```by
class Counter:
    private start = 1
    step = start + 1

    private def helper(self) -> int:
        return self.step

    alias = helper

reveal_type(Counter.step)  # revealed: int
```

## a declaration inside a compound statement in the class body

A declaration written under an `if` or a `try` is as much the class's member as one written at the
top of the body.

```by
import sys

class A:
    if sys.version_info >= (3, 8):
        private x: int = 1

A().x  # error: [inaccessible-member]
```

## a decorated private method is still private

The method's visibility is read off its declaration, not off whatever type a decorator turns it
into.

```by
import functools

class A:
    @functools.cache
    private def cached(self) -> int:
        return 1

    def use(self) -> None:
        self.cached()

def outside(a: A) -> None:
    a.cached()  # error: [inaccessible-member]
```

## a private nested class

```by
class A:
    private class Inner: ...

    def make(self) -> None:
        self.Inner()

A.Inner  # error: [inaccessible-member]
```

## a protected member reached through a union

Every class a union can be declares the member the same way, so it has one name to be reached by.

```by
class A:
    protected x: int = 1

class B(A): ...

class C(A):
    def other(self, o: B | C) -> int:
        return o.x

reveal_type(C().other(B()))  # revealed: int
```

## a protected member one class of a union overrides

`B` overrides `x` and `C` inherits it from `A`, but both emit it as `_x`, so the union has one name
to reach it by. Each class's declaration is still checked, and `D` is a subclass of both `B` and
`A`.

```by
class A:
    protected def x(self) -> int:
        return 1

class B(A):
    protected override def x(self) -> int:
        return 2

class C(A): ...

class D(B):
    def other(self, o: B | C) -> int:
        return o.x()

reveal_type(D().other(C()))  # revealed: int
```

## an override of a protected member is protected to the overriding class

The member `o.x` reaches is `B`'s, and `C` is not a subclass of `B`.

```by
class A:
    protected def x(self) -> int:
        return 1

class B(A):
    protected override def x(self) -> int:
        return 2

class C(A):
    def other(self, o: B) -> int:
        # error: [inaccessible-member] "`x` is protected: only `B` and its subclasses may reach it"
        return o.x()
```

## a union whose classes restrict a member differently

`A` emits `x` as `_x` and `D` emits it as `x`, so no one attribute access reaches both.

```by
class A:
    protected x: int = 1

    def read(self, o: A | D) -> None:
        # error: [inaccessible-member] "`x` is emitted under a different name on the classes `A | D` can be"
        o.x

class D:
    x: int = 2
```

## `super()` reaches a protected member

```by
class A:
    protected class var x: int = 1

class B(A):
    def read(self) -> int:
        return super().x

reveal_type(B().read())  # revealed: int
```

## `__slots__` names a private member by its name

Python mangles a `__slots__` entry the way it mangles the class body's own names, so the string is
renamed with the declaration.

```by
class A:
    __slots__ = ("x",)

    init(private let x: int)

    def get(self) -> int:
        return self.x

reveal_type(A(1).get())  # revealed: int
```

## a class pattern's keyword reads the member it names

`case A(x=1)` reads `x` off the subject, so it is renamed and checked like `a.x`.

```by
class A:
    __match_args__ = ("x",)

    init(private let x: int)

    def matches(self) -> bool:
        match self:
            case A(x=1):
                return True
        return False

reveal_type(A(1).matches())  # revealed: bool
```

## a class pattern's keyword is checked like an access

```by
class A:
    init(private let x: int)

def f(a: A) -> None:
    match a:
        # error: [inaccessible-member]
        case A(x=1):
            pass
```

## `protected` is an ordinary name in python

The keyword only means something where a modifier chain is written, so everywhere else it is a name
like any other.

```py
protected = [1]
protected.append(2)
```

## a visibility keyword on a local is an error

A declaration in a function body is a local, which nothing outside the function reaches anyway.

```by
def f() -> int:
    private x = 1  # error: [invalid-syntax]
    return x
```

## widening a protected member is not an override

`A.f` is emitted as `_f` and `B.f` as `f`, so `B.f` sits beside the inherited method rather than
replacing it.

```by
class A:
    protected def f(self) -> int:
        return 1

class B(A):
    # error: [invalid-override-visibility]
    def f(self) -> int:
        return 2
```

## a dataclass field cannot be private

The field's name is its constructor's keyword.

```by
from dataclasses import dataclass

@dataclass
class Point:
    # error: [invalid-visibility]
    private x: int
```

## a named tuple's field cannot be protected

```by
from typing import NamedTuple

class P(NamedTuple):
    # error: [invalid-visibility]
    protected x: int
```

## a typed dict's key cannot be private

```by
from typing import TypedDict

class TD(TypedDict):
    # error: [invalid-visibility]
    private x: int
```

## an enum member cannot be private

```by
from enum import Enum

class E(Enum):
    # error: [invalid-visibility]
    private A = 1
```

## an abstract method cannot be private

A `private` method is renamed per class, so no subclass could ever override it.

```by
from abc import ABC, abstractmethod

class A(ABC):
    @abstractmethod
    # error: [invalid-visibility]
    private def f(self) -> int: ...
```

## `protected` on a name python already mangles

```by
class A:
    # error: [ineffective-private]
    protected __x: int = 1
```

## `private` in a class named only with underscores

Python mangles nothing in such a class, so `private` would hide nothing.

```by
class __:
    # error: [ineffective-private]
    private x: int = 1
```

## a module-level private variable declared under an `if`

```by
import sys

if sys.version_info >= (3, 8):
    private flag = True

def read() -> bool:
    return flag

reveal_type(read())  # revealed: bool
```

## a class body reads the module's private variable before binding its own

Python reads a class's own binding only once the body has made it, so the right-hand side here is
the module's `count`.

```by
private count = 1

class H:
    count = count + 1

reveal_type(H.count)  # revealed: int
```

## a class body that may or may not have bound the name

The read is the class's own `count` on one path and the module's private one on the other, and no
single emitted name reads both.

```by
import random

private count = 1

class H:
    if random.random() > 0.5:
        count = 2
    # error: [invalid-visibility]
    x = count
```

## a star import leaves private symbols out

The lowering renames a private symbol with a leading underscore, so python's own rule leaves it out
of `from helpers import *`.

`helpers.by`:

```by
private hidden = 1
visible = 2
```

`main.by`:

```by
from helpers import *

reveal_type(visible)  # revealed: 2
hidden  # error: [unresolved-reference]
```

## a private symbol cannot be listed in `__all__`

```by
private def helper() -> int:
    return 1

__all__ = ["helper"]  # error: [private-export]
```

## a dotted import cannot rebind a private symbol

`import os.path` binds the package `os`, which no alias can keep under the underscored name.

```by
private os = 1

import os.path  # error: [invalid-visibility]
```
