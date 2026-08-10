# conformance

an argument list on an extension declares that the extended type conforms to those interfaces. the
block supplies whatever the interface asks for and the type does not already answer

## a conformance makes an existing type satisfy a protocol

```by
protocol Show:
    def show(self) -> str

extension str(Show):
    override def show(self) -> str:
        return self

def render(value: Show) -> str:
    return value.show()

reveal_type(render("hi"))  # revealed: str
```

## a type with no conformance is still rejected

```by
protocol Show:
    def show(self) -> str

extension str(Show):
    override def show(self) -> str:
        return self

def render(value: Show) -> str:
    return value.show()

# error: [invalid-argument-type]
render(1)
```

## a protocol extension's members reach every conforming type

an `extension <protocol>:` adds inherent members to the protocol, and a conformance carries them to
the conforming type

```by
protocol Show:
    def show(self) -> str

extension Show:
    def shout(self) -> str:
        return self.show().upper()

extension str(Show):
    override def show(self) -> str:
        return self

reveal_type("hi".shout())  # revealed: str

def render(value: Show) -> str:
    return value.shout()
```

## without a conformance the protocol's extension does not apply

```by
protocol Show:
    def show(self) -> str

extension Show:
    def shout(self) -> str:
        return self.show().upper()

# error: [unresolved-attribute]
reveal_type("hi".shout())  # revealed: Unknown
```

## the conformance block's own members resolve on the type

```by
protocol Show:
    def show(self) -> str

extension str(Show):
    override def show(self) -> str:
        return self

reveal_type("hi".show())  # revealed: str
```

## a requirement nothing answers is reported

```by
protocol Show:
    def show(self) -> str

# error: [invalid-conformance] "`str` does not answer every member of `Show`"
extension str(Show): ...
```

## a default on the protocol's extension answers the requirement

```by
protocol Show:
    def show(self) -> str

extension Show:
    def show(self) -> str:
        return "?"

extension str(Show): ...

def render(value: Show) -> str:
    return value.show()

reveal_type(render("hi"))  # revealed: str
```

## a type that already answers the requirement needs no member

```by
protocol Sized:
    def __len__(self) -> int

extension str(Sized): ...

def size(value: Sized) -> int:
    return len(value)

reveal_type(size("hi"))  # revealed: int
```

## the interface must be a protocol

```by
class Concrete:
    x: int = 0

# error: [invalid-conformance] "`Concrete` is not a protocol"
extension str(Concrete): ...
```

## an abstract class is not an interface a type can conform to

an abstract class carries concrete members a conformance could never answer, and it already has
inheritance and `register` for the job

```by
from abc import ABC, abstractmethod

class Show(ABC):
    @abstractmethod
    def show(self) -> str: ...

# error: [invalid-conformance] "`Show` is not a protocol"
extension str(Show):
    override def show(self) -> str:
        return self
```

## a conformance may name more than one interface

```by
protocol Show:
    def show(self) -> str

protocol Size:
    def size(self) -> int

extension str(Show, Size):
    override def show(self) -> str:
        return self

    override def size(self) -> int:
        return len(self)

def render(value: Show) -> str:
    return value.show()

def measure(value: Size) -> int:
    return value.size()

reveal_type(render("hi"))  # revealed: str
reveal_type(measure("hi"))  # revealed: int
```

## an ordinary extension declares no conformance

```by
protocol Show:
    def show(self) -> str

extension str:
    def show(self) -> str:
        return self

def render(value: Show) -> str:
    return value.show()

# error: [invalid-argument-type]
render("hi")
```

## conformance narrows an `object` and dispatches on it

```by
protocol Show:
    def show(self) -> str

extension Show:
    def shout(self) -> str:
        return self.show().upper()

extension str(Show):
    override def show(self) -> str:
        return self

def describe(value: object) -> str:
    if value is Show:
        reveal_type(value)  # revealed: Show
        return value.shout() + value.show()
    return ""
```

## a conformance in an imported module is applicable here

`iface.by`:

```by
protocol Show:
    def show(self) -> str
```

`adapters.by`:

```by
from iface import Show

extension str(Show):
    override def show(self) -> str:
        return self
```

`main.by`:

```by
import adapters
from iface import Show

def render(value: Show) -> str:
    return value.show()

reveal_type(render("hi"))  # revealed: str
```

## without the import the conformance is not applicable

`iface2.by`:

```by
protocol Show:
    def show(self) -> str
```

`adapters2.by`:

```by
from iface2 import Show

extension str(Show):
    override def show(self) -> str:
        return self
```

`main2.by`:

```by
from iface2 import Show

def render(value: Show) -> str:
    return value.show()

# error: [invalid-argument-type]
render("hi")
```

## two conformances of one pair are reported

which witness table survives would depend on import order, so the second is rejected at its own
declaration

```by
protocol Show:
    def show(self) -> str

extension str(Show):
    override def show(self) -> str:
        return self

# error: [invalid-conformance] "`str` is already conformed to `Show` here"
extension str(Show):
    override def show(self) -> str:
        return self.upper()
```

## a member that does not match the requirement is reported

every call through the interface goes to it, and the extension's members are written against the
extended type rather than inherited from the interface — so nothing else would catch this

```by
protocol Show:
    def show(self) -> str

# error: [invalid-conformance] "`show` does not match the member `Show` declares"
extension str(Show):
    override def show(self) -> int:
        return 1
```

## a matching member without `override` is accepted

```by
protocol Show:
    def show(self) -> str

class Widget: ...

extension Widget(Show):
    def show(self) -> str:
        return "w"

def render(value: Show) -> str:
    return value.show()

reveal_type(render(Widget()))  # revealed: str
```

## a conformance may not carry a bracket bound

the registry is keyed by class, so a bound could not be checked where a value is dispatched on —
`list[str]` would get the `list[int]` witness

```by
protocol Show:
    def show(self) -> str

# error: [invalid-conformance] "a conformance may not carry a bracket bound"
extension list[Element: int](Show):
    override def show(self) -> str:
        return ", ".join(str(x) for x in self)
```

## a conformance cannot supply a dunder

an ordinary extension may supply an operator's dunder, since that is rewritten from the concrete
operand type — but a requirement is reached through the interface, where the concrete type is what
is unknown

```by
protocol Sized:
    def __len__(self) -> int

class Bag: ...

# error: [invalid-conformance] "a conformance cannot supply `__len__`"
extension Bag(Sized):
    override def __len__(self) -> int:
        return 0
```

## the same dunder in an extension with no conformance is fine

```by
class Money:
    cents: int = 0

extension Money:
    def __add__(self, other: Money) -> Money:
        return Money()

reveal_type(Money() + Money())  # revealed: Money
```

## a dunder the type already has needs no witness

```by
protocol Sized:
    def __len__(self) -> int

extension str(Sized): ...

def size(value: Sized) -> int:
    return len(value)

reveal_type(size("hi"))  # revealed: int
```

## a conformance declared above what it names is reported

```by
protocol Show:
    def show(self) -> str

# error: [invalid-conformance] "`Widget` is declared after this conformance"
extension Widget(Show):
    override def show(self) -> str:
        return "w"

class Widget: ...
```

## a conformance's own member overrides the interface's default

```by
protocol Show:
    def show(self) -> str

extension Show:
    def show(self) -> str:
        return "default"

extension str(Show):
    override def show(self) -> str:
        return "own"

reveal_type("hi".show())  # revealed: str

def render(value: Show) -> str:
    return value.show()
```

## a member the type already has is shape-checked too

nothing else would catch this: the conformance supplies no member, so there is no `override` for the
inheritance machinery to look at

```by
protocol Show:
    def show(self) -> str

class Widget:
    def show(self) -> int:
        return 1

# error: [invalid-conformance] "`show` does not match the member `Show` declares"
extension Widget(Show): ...
```

## a requirement `object` already answers needs no member

```by
protocol Stringy:
    def __str__(self) -> str

class Widget: ...

extension Widget(Stringy): ...

def label(value: Stringy) -> str:
    return str(value)
```

## a base that is not a class is reported on its own span

```by
protocol Show:
    def show(self) -> str

NotAClass = 1

# error: [invalid-conformance] "a conformance list names interfaces"
extension str(NotAClass, Show):
    override def show(self) -> str:
        return self
```
