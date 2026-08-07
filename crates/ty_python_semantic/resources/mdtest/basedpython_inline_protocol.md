# basedpython: inline protocol type expressions

In basedpython, `protocol(a: int; def f(self) -> int)` in a type position is an anonymous structural
protocol. Members are separated by `;`: `name: T` declares a data member and `def name(...) -> T`
declares a method member, whose receiver binds away on access the way a method defined in a
`Protocol` class body does.

The result is a structural type with no identity of its own, so two occurrences of the same members
are the same type wherever they are written. The transpiler hoists each shape to a synthesized
`typing.Protocol` subclass.

## Members

### Data members

```by
def f(x: protocol(a: int; b: str)) -> None:
    reveal_type(x.a)  # revealed: int
    reveal_type(x.b)  # revealed: str
```

### Method members

A method member's first parameter is the receiver — a parameter *name*, not a type — so it is bound
away when the member is accessed.

```by
def f(x: protocol(def m(self) -> str)) -> None:
    reveal_type(x.m)  # revealed: () -> str
    reveal_type(x.m())  # revealed: str
```

### A method member is not a data member of callable type

Every parameter after the receiver keeps the ordinary callable-arrow meaning, where a bare name is a
positional-only parameter's type and `name: T` is a named parameter.

```by
def f(x: protocol(def m(self, n: int) -> str)) -> None:
    reveal_type(x.m)  # revealed: (n: int) -> str
    reveal_type(x.m(1))  # revealed: str

def g(x: protocol(m: (int) -> str)) -> None:
    # a data member of callable type is not bound, so it keeps its parameter
    reveal_type(x.m)  # revealed: (int, /) -> str
```

### The first parameter is always the receiver

A bare name in the first position is the receiver, so a method with no parameters of its own must
still spell it. Written without one, `def m(int) -> str` declares a receiver *named* `int` rather
than a positional-only `int` parameter — every *later* bare name keeps the callable-arrow meaning.

```by
def f(x: protocol(def m(int) -> str; def n(self, int) -> str)) -> None:
    reveal_type(x.m)  # revealed: () -> str
    reveal_type(x.n)  # revealed: (int, /) -> str
```

### Full parameter spec

```by
def f(x: protocol(def m(self, a: int, /, *args: str, **kw: bytes) -> None)) -> None:
    reveal_type(x.m)  # revealed: (a: int, /, *args: str, **kw: bytes) -> None
```

### Written across several lines, with a trailing `;`

```by
def f(
    x: protocol(
        a: int;
        def m(self) -> str;
    ),
) -> None:
    reveal_type(x.a)  # revealed: int
    reveal_type(x.m())  # revealed: str
```

## Assignability

### A class satisfies an inline protocol structurally

A class satisfies an inline protocol structurally — it does not have to inherit anything.

```by
class Impl:
    a: int

    def m(self) -> str:
        return ""

class MissingMethod:
    a: int

def f(x: protocol(a: int; def m(self) -> str)) -> None: ...

f(Impl())

# error: [invalid-argument-type] "Argument to function `f` is incorrect"
f(MissingMethod())
```

### Two inline protocols with the same members are the same type

```by
def f(x: protocol(a: int)) -> None: ...

def g(y: protocol(a: int)) -> None:
    f(y)
```

### A protocol with fewer members accepts one with more

```by
def narrow(x: protocol(a: int)) -> None: ...

def wide(y: protocol(a: int; b: str)) -> None:
    narrow(y)
```

## Type variables in a member

An inline protocol is not a generic class of its own, but its members can mention the type variables
of the scope it is written in. Specializing that scope substitutes them.

```by
class A[T]:
    def get(self) -> protocol(a: T):
        raise NotImplementedError

def f(x: A[int]) -> None:
    reveal_type(x.get())  # revealed: protocol(a: int)
    reveal_type(x.get().a)  # revealed: int
```

## A method member keeps its receiver

### A call on a type parameter bounded by an inline protocol stays symbolic

A method member's receiver binds away, but the member still names *that* receiver's method, so a
call on a type parameter is the symbolic `T.m()` rather than the return type the protocol declares.
Specializing the parameter re-resolves the call against whatever it was specialized to.

```by
class B:
    def foo(self) -> int:
        return 1

class X:
    def foo(self) -> B:
        return B()

def f[T: protocol(def foo(self) -> B)](t: T):
    return t.foo()

reveal_type(f)  # revealed: def f[T](t: T) -> T.foo()
reveal_type(f(X()))  # revealed: B
```

### A class-based protocol bound answers the same way

The two spellings of the same interface have to agree: `Protocol` class or `protocol(...)`, a call
on the parameter names the same operation.

```by
from typing import Protocol

class B:
    def foo(self) -> int:
        return 1

class HasFoo(Protocol):
    def foo(self) -> B: ...

class X:
    def foo(self) -> B:
        return B()

def f[T: HasFoo](t: T):
    return t.foo()

reveal_type(f)  # revealed: def f[T](t: T) -> T.foo()
reveal_type(f(X()))  # revealed: B
```

### A chain of calls composes

Each link is a call on the value the one before it produced, so the whole chain stays one symbolic
expression until the parameter is specialized.

```by
class B:
    def foo(self) -> int:
        return 1

class A:
    def foo(self) -> B:
        return B()

class X:
    def foo(self) -> A:
        return A()

def f[T: protocol(def foo(self) -> protocol(def foo(self) -> B))](t: T):
    return t.foo().foo()

reveal_type(f)  # revealed: def f[T](t: T) -> T.foo().foo()
reveal_type(f(X()))  # revealed: B
```

### A chain composes through a local too

A local the chain was stepped through holds the same symbolic value the expression had, so writing
the links out one at a time reaches the same place.

```by
class B:
    def foo(self) -> int:
        return 1

class A:
    def foo(self) -> B:
        return B()

class X:
    def foo(self) -> A:
        return A()

def f[T: protocol(def foo(self) -> protocol(def foo(self) -> B))](t: T):
    a = t.foo()
    return a.foo()

reveal_type(f)  # revealed: def f[T](t: T) -> T.foo().foo()
reveal_type(f(X()))  # revealed: B
```

### An attribute member reads as its declared type

Only a method needs its receiver kept. A data member — and the read-only member an inferred protocol
asks for — reads back as its declared type through a `Protocol` class too, so both spellings already
agree about it.

```by
from typing import Protocol

class HasA(Protocol):
    a: int

def inline[T: protocol(a: int)](t: T):
    return t.a

def klass[T: HasA](t: T):
    return t.a

reveal_type(inline)  # revealed: def inline[T](t: T) -> int
reveal_type(klass)  # revealed: def klass[T](t: T) -> int
```

## Keyword unpacks

### A pack contributes one data member per field

`protocol(**Kwargs)` splices a [keyword-variadic pack](basedpython_keyword_variadic.md) into the
member list. The pack contributes one data member per field, once it is specialized.

```by
class A[**Kwargs]:
    def __init__(self, **kwargs: **Kwargs) -> None: ...
    def get(self) -> protocol(**Kwargs): ...

def check() -> None:
    a = A(foo=1, bar="x")
    reveal_type(a.get().foo)  # revealed: int
    reveal_type(a.get().bar)  # revealed: str
```

### A pack composes with members written out longhand

```by
class A[**Kwargs]:
    def __init__(self, **kwargs: **Kwargs) -> None: ...
    def tagged(self) -> protocol(tag: int; **Kwargs):
        raise NotImplementedError

def check() -> None:
    a = A(foo=1)
    reveal_type(a.tagged().tag)  # revealed: int
    reveal_type(a.tagged().foo)  # revealed: int
```

### An unspecialized pack is carried until it is specialized

```by
class A[**Kwargs]:
    def get(self) -> protocol(**Kwargs): ...

    def use(self) -> None:
        reveal_type(self.get())  # revealed: protocol(**Kwargs@A)
```

### An unspecialized pack declares no members yet

Until the pack is specialized its fields are unknown, so the protocol requires nothing. A protocol
in a parameter position therefore accepts any argument while the enclosing scope is still generic —
the requirement only materializes at the specialization site.

```by
class A[**Kwargs]:
    def __init__(self, **kwargs: **Kwargs) -> None: ...
    def take(self, p: protocol(**Kwargs)) -> None: ...

    def inner(self) -> None:
        self.take(42)

def check() -> None:
    a = A(foo=1)
    # error: [invalid-argument-type] "Argument to bound method `A.take` is incorrect"
    a.take(42)
```

### Only a keyword-variadic pack can be unpacked

```by
# error: [invalid-type-form] "Only a keyword-variadic pack can be unpacked into an inline protocol, not `int`"
def f(x: protocol(**int)) -> None: ...
```

## Display

A synthesized protocol reads back as the members it declares.

```by
def f(x: protocol(a: int; def m(self) -> str)) -> None:
    reveal_type(x)  # revealed: protocol(a: int; def m(self) -> str)
```

## `protocol` is a soft keyword

A call to something named `protocol` still parses as a call — only a parenthesized list whose first
member is a `def`, a `**` unpack, or a `name: T` declaration is an inline protocol.

```by
def protocol(x: int) -> str:
    return ""

reveal_type(protocol(1))  # revealed: str
```

## Not valid in a `.py` file

```python
# error: [invalid-syntax] "inline protocol type `protocol(...)` is not valid in .py files"
def f(x: protocol(a: int)) -> None: ...
```

## A duplicate member is rejected

Member names are labels, so a duplicate would silently drop one of the two declarations.

```by
# error: [invalid-syntax] "duplicate protocol member `a`"
def f(x: protocol(a: int; a: str)) -> None: ...
```
