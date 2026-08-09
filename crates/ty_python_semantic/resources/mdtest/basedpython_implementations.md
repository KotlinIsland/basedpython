# basedpython implementations

`implementation A for B:` states retroactively that `B` satisfies `A`. The block is a witness class
deriving the interface, and `B` is made acceptable where an `A` is asked for by a *conversion* at
the positions where the transpiler can materialize that witness — not by a subtyping edge.

## a conversion site accepts the implemented type

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B:
    override def f(self) -> int:
        return self.a

def takes_a(a: A) -> int:
    return a.f()

takes_a(B())
```

## an annotated assignment is a conversion site

The declared type is right there and the value is one expression, so a witness can be materialized —
the same two conditions a call argument satisfies.

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B:
    override def f(self) -> int:
        return self.a

a: A = B()
reveal_type(a)  # revealed: A
```

## a `from ... import` makes an implementation applicable

Importing the interface and the implemented type by name is the natural way to write this, so it is
what establishes the dependency — requiring a separate `import mod` whose symbols are never used
would leave an import that reads as removable.

`iface.by`:

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 7
```

`adapters.by`:

```by
from iface import A, B

implementation A for B:
    override def f(self) -> int:
        return self.a
```

`main.by`:

```by
from adapters import A
from iface import B

def takes_a(x: A) -> int:
    return x.f()

takes_a(B())

value: A = B()
```

## every conversion site

A conversion happens wherever the checker checks an expression against a declared type and the
transpiler can wrap that expression: a call argument, an annotated assignment, an attribute
assignment, a `return`, and — element-wise — a collection literal or comprehension.

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B:
    override def f(self) -> int:
        return self.a

class Holder:
    field: A

def takes_a(x: A) -> int:
    return x.f()

def ret() -> A:
    return B()

def sites(h: Holder, c: bool, bs: list[B]) -> None:
    takes_a(B())
    value: A = B()
    h.field = B()
    arm: A = B() if c else B()
    xs: list[A] = [B(), B()]
    ys: list[A] = [b for b in bs]
    d: dict[str, A] = {"k": B()}
    t: tuple[A, A] = (B(), B())
    s: set[A] = {B()}
```

## a collection that is not a literal still does not convert

The element-wise conversion is only available where the elements are in the source. A variable
already holding a `list[B]` would need an O(n) copy with different identity, so it stays an error —
this is what keeps conformance out of the type lattice.

```by
from typing import Sequence

abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B as BAsA:
    override def f(self) -> int:
        return self.a

def takes_seq(items: Sequence[A]) -> None: ...

xs: list[B] = [B()]

# error: [invalid-argument-type] "Expected `Sequence[A]`, found `list[B]`"
takes_seq(xs)

# error: [invalid-assignment] "Object of type `list[B]` is not assignable to `list[A]`"
ys: list[A] = xs
```

## a literal with an unpacked element does not convert

An unpacked element comes from another collection, so it has no expression of its own to wrap.

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B:
    override def f(self) -> int:
        return self.a

bs: list[B] = [B()]

# error: [invalid-assignment] "Object of type `list[A | B]` is not assignable to `list[A]`"
xs: list[A] = [B(), *bs]
```

## a plain assignment to a declared name converts

The declared type lives in another statement, and the name carries it to the assignment.

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B:
    override def f(self) -> int:
        return self.a

def f() -> None:
    x: A
    x = B()
    reveal_type(x)  # revealed: A
    y: A = B()
```

## the implemented type's members are reachable through `self`

A witness forwards what it does not itself define to the object it wraps, so an implementation body
reads `B`'s state directly — declared attributes, class-level values, and attributes assigned in
`__init__` alike.

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    declared: int
    valued: int = 1

    def __init__(self):
        self.assigned = 2

implementation A for B:
    override def f(self) -> int:
        reveal_type(self.declared)  # revealed: int
        reveal_type(self.valued)  # revealed: int
        reveal_type(self.assigned)  # revealed: int
        return 0
```

## `__implemented__` is the wrapped object

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

def takes_b(b: B) -> int:
    return b.a

implementation A for B:
    override def f(self) -> int:
        return takes_b(self.__implemented__)
```

## a named implementation binds its witness class

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B as BAsA:
    override def f(self) -> int:
        return self.a

b = B()
reveal_type(BAsA(b))  # revealed: final BAsA
reveal_type(BAsA(b).a)  # revealed: int
reveal_type(BAsA(b).__implemented__)  # revealed: B

def takes_a(a: A) -> int:
    return a.f()

takes_a(BAsA(b))
```

## a witness is not the implemented type

The asymmetry is the safety story: a witness is a distinct object at runtime, so letting it flow
into a `B` position would hand out something whose `type()` and `isinstance` answers are wrong.

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B as BAsA:
    override def f(self) -> int:
        return self.a

def takes_b(b: B) -> int:
    return b.a

# error: [invalid-argument-type] "Expected `B`, found `final BAsA`"
takes_b(BAsA(B()))
```

## conformance does not reach inside a generic

`list[B]` is not a `Sequence[A]`: making it one would mean wrapping every element, an O(n) copy with
different identity hidden behind a call. Convert explicitly instead.

```by
from typing import Sequence

abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B as BAsA:
    override def f(self) -> int:
        return self.a

def takes_seq(items: Sequence[A]) -> None: ...

xs: list[B] = [B()]

# error: [invalid-argument-type] "Expected `Sequence[A]`, found `list[B]`"
takes_seq(xs)

takes_seq([BAsA(x) for x in xs])
```

## an unimplemented abstract member is an error

Reported at the header: an anonymous implementation is never instantiated in source, so there is no
call site for the ordinary abstract-instantiation error to land on. A member with a default body may
be omitted.

```by
abstract class A:
    abstract def supplied(self) -> int: ...
    abstract def missing(self) -> int: ...
    abstract def defaulted(self) -> int:
        return 0

class B:
    a: int = 3

# error: [invalid-implementation] "`B` does not implement every abstract member of `A`"
implementation A for B as BAsA:
    override def supplied(self) -> int:
        return self.a
```

## an unpacked argument is not a conversion site

The transpiler wraps a whole argument expression; `*args` feeds parameters the call site has no
separate expression for, so there is nothing to wrap and the conversion is declined rather than
accepted and silently skipped.

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B as BAsA:
    override def f(self) -> int:
        return self.a

def g(x: A) -> int:
    return x.f()

args = (B(),)

# error: [invalid-argument-type] "Expected `A`, found `final B`"
g(*args)

# the explicit form works
g(*(BAsA(B()),))
```

## an implementation must be declared at module level

Only module-level implementations are enumerated and lowered, so a nested one would type-check and
then leak its surface syntax into the output.

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

def scope() -> None:
    # error: [invalid-implementation] "an `implementation` must be declared at module level"
    implementation A for B:
        override def f(self) -> int:
            return self.a
```

## an interface that needs a constructor cannot be implemented

A witness holds the implemented object and never runs the interface's `__init__`, so state assigned
there would silently never exist.

```by
abstract class Stateful:
    def __init__(self, label: str):
        self.label = label
    abstract def f(self) -> str: ...

class B:
    a: int = 3

# error: [invalid-implementation] "`Stateful` defines `__init__`, so it cannot be implemented"
implementation Stateful for B:
    override def f(self) -> str:
        return "x"
```

## a valueless declaration on the interface must be supplied

An annotation with no value has no runtime existence on the interface — only its constructor would
assign it, and a witness never runs one.

```by
abstract class Labelled:
    label: str
    abstract def f(self) -> str: ...

class B:
    a: int = 3

# error: [invalid-implementation] "`Labelled` declares `label` without a value, so this implementation must supply it"
implementation Labelled for B:
    override def f(self) -> str:
        return self.label
```

## supplying a valueless declaration completes the implementation

```by
abstract class Labelled:
    label: str
    abstract def f(self) -> str: ...

class B:
    a: int = 3

implementation Labelled for B:
    label = "b"

    override def f(self) -> str:
        return self.label
```

## the interface must be an abstract class or a protocol

```by
class Concrete:
    x: int

class B:
    a: int = 3

# error: [invalid-implementation] "`Concrete` is not an abstract class or a protocol"
implementation Concrete for B:
    # error: [invalid-explicit-override]
    override def f(self) -> int:
        return 0
```

## the implemented name must be a class

```by
abstract class A:
    abstract def f(self) -> int: ...

# error: [invalid-implementation] "`Missing` is not a class; an implementation must name an existing class to implement the interface for"
implementation A for Missing:
    override def f(self) -> int:
        return 0
```

## a member matching nothing on the interface is an error

An implementation promises conformance; an `extension` adds inherent members.

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B:
    override def f(self) -> int:
        return self.a

    # error: [invalid-implementation] "`A` declares no member `helper`"
    def helper(self) -> int:
        return 0
```

## a type that already satisfies the interface is an error

No conversion would ever fire, so the block would be dead code.

```by
abstract class A:
    abstract def f(self) -> int: ...

class Already(A):
    override def f(self) -> int:
        return 0

# error: [invalid-implementation] "`Already` already satisfies `A`"
implementation A for Already:
    override def f(self) -> int:
        return 1
```

## a second implementation of the same pair is an error

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B:
    override def f(self) -> int:
        return 0

# error: [invalid-implementation] "`A` is already implemented for `B` in this module"
implementation A for B:
    override def f(self) -> int:
        return 1
```

## importing a module makes its implementations applicable

`iface.by`:

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 7
```

`adapters.by`:

```by
from iface import A, B

implementation A for B:
    override def f(self) -> int:
        return self.a
```

`main.by`:

```by
import adapters
from iface import A, B

def takes_a(a: A) -> int:
    return a.f()

takes_a(B())
```

## without the import, there is no conversion

The conversion, not just a member lookup, is what the import gates: the assignment itself fails.

`iface.by`:

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 7
```

`adapters.by`:

```by
from iface import A, B

implementation A for B:
    override def f(self) -> int:
        return self.a
```

`main.by`:

```by
from iface import A, B

def takes_a(a: A) -> int:
    return a.f()

# error: [invalid-argument-type] "Expected `A`, found `final B`"
takes_a(B())
```

## two imported modules implementing the same pair is an error

`iface.by`:

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 7
```

`one.by`:

```by
from iface import A, B

implementation A for B:
    override def f(self) -> int:
        return 1
```

`two.by`:

```by
from iface import A, B

implementation A for B:
    override def f(self) -> int:
        return 2
```

`main.by`:

```by
import one
import two
from iface import A, B

def takes_a(a: A) -> int:
    return a.f()

# error: [ambiguous-conversion] "More than one applicable implementation converts `B` here"
takes_a(B())
```

## an implementation binds no name for the implemented type

An anonymous implementation's header names the implemented type by reference; it must not shadow it.

```by
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

implementation A for B:
    override def f(self) -> int:
        return self.a

reveal_type(B)  # revealed: <class 'B'>
reveal_type(B())  # revealed: final B
```

## a protocol interface

A protocol whose members the type does not already have, adapted by an implementation.

```by
from typing import Protocol

class Show(Protocol):
    def show(self) -> str: ...

class Widget:
    label: str = "w"

implementation Show for Widget:
    override def show(self) -> str:
        return self.label

def render(item: Show) -> str:
    return item.show()

render(Widget())
```
