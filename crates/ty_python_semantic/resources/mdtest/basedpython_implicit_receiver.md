# basedpython: implicit receivers

a callable type may declare a *receiver* — `int.() -> str` is a callable whose first argument is the
receiver it runs against. the receiver is an ordinary leading positional parameter, so any function
of the same shape satisfies it, and it additionally unlocks two forms: calling the callable as a
method of the receiver (`x.fn()`), and a trailing lambda block whose body sees the receiver's
members unqualified and spells the receiver itself `self`

```toml
[environment]
python-version = "3.12"
```

## the receiver is a leading positional parameter

```by
def apply(fn: int.() -> str) -> str:
    return fn(1)

def render(value: int) -> str:
    return str(value)

reveal_type(apply(render))  # revealed: str
```

## calling through the receiver

a name in scope whose declared type is a receiver callable can be called as a method of a matching
receiver. the callable is bound to it, so the receiver parameter is already supplied

```by
def apply(fn: int.() -> str) -> None:
    receiver = 1
    reveal_type(receiver.fn)  # revealed: () -> str
    reveal_type(receiver.fn())  # revealed: str
```

it applies to any expression of the receiver's type, not just a local

```by
def apply(fn: str.(int) -> bytes) -> None:
    reveal_type("abc".fn(1))  # revealed: bytes
```

## a declared member always wins

an implicit receiver never shadows a real member of the receiver type

```by
def apply(bit_length: int.() -> str) -> None:
    # `int.bit_length` is a real method, so it is unaffected
    reveal_type((1).bit_length())  # revealed: int
```

## the receiver must match

```by
def apply(fn: int.() -> str) -> None:
    receiver = "abc"
    receiver.fn  # error: [unresolved-attribute]
```

## a binding shadows a receiver callable

resolution follows python's own scoping: the first scope that gives the name a value decides it, so
a local binding shadows an outer receiver callable rather than deferring to it

```by
renderer: int.() -> str

def use() -> None:
    renderer = "shadowed"
    x = 1
    x.renderer  # error: [unresolved-attribute]
```

the name must be *declared* for the same reason — a plain binding carries no receiver callable

```by
def render(value: int) -> str:
    return str(value)

def use() -> None:
    fn = render
    x = 1
    x.fn  # error: [unresolved-attribute]
```

## calling a receiver callable directly needs the receiver

```by
def apply(fn: int.() -> str) -> None:
    fn()  # error: [missing-argument] "No argument provided for required parameter 1 (the receiver)"
```

## a plain callable has no receiver

```by
def apply(fn: (int) -> str) -> None:
    receiver = 1
    receiver.fn  # error: [unresolved-attribute]
```

## trailing lambda blocks

a trailing lambda block bound to a receiver callable sees the receiver's members unqualified, and
spells the receiver itself `self`. the block's implicit `it` parameter is the callback's *own*
argument — the one after the receiver.

the receiver joins the scope tower at the block's own level: inside the names the block itself
binds, and outside everything else

### the receiver's members are in scope

```by
def apply(fn: int.() -> None) -> None:
    fn(1)

apply:
    reveal_type(self)  # revealed: int
    reveal_type(imag)  # revealed: 0
    reveal_type(bit_length())  # revealed: int
```

### a receiver callback's argument is `it`

```by
def apply(fn: int.(str) -> None) -> None:
    fn(1, "a")

apply:
    reveal_type(self)  # revealed: int
    reveal_type(it)  # revealed: str
    reveal_type(bit_length())  # revealed: int
```

### the receiver outranks a module global

```by
def apply(fn: int.() -> None) -> None:
    fn(1)

imag: str = "shadow"

apply:
    reveal_type(imag)  # revealed: 0
```

### the receiver outranks an enclosing function's local

```by
def apply(fn: int.() -> None) -> None:
    fn(1)

def enclosing() -> None:
    imag: str = "local"
    apply:
        reveal_type(imag)  # revealed: 0
```

### the receiver outranks a builtin

```by
class Formatter:
    def format(self, value: int) -> str:
        return str(value)

def apply(fn: Formatter.() -> None) -> None: ...

apply:
    reveal_type(format(1))  # revealed: str
```

### the receiver is what `self` means

a method's own `self` is outside the block, so it no longer reaches into it

```by
def apply(fn: int.() -> None) -> None:
    fn(1)

class C:
    def m(self) -> None:
        apply:
            reveal_type(self)  # revealed: int
```

### a name the block declares keeps its own meaning

the block itself is the one level of the tower inside the receiver, and a declaration is how the
block asks for that level. it shadows the receiver's member of the same name, which is worth saying
out loud since a bare assignment to that name means the opposite

```by
def apply(fn: int.() -> None) -> None:
    fn(1)

apply:
    let imag = "block"  # error: [shadowed-receiver-member]
    reveal_type(imag)  # revealed: "block"
```

a name the receiver has nothing to answer for is an ordinary local, declared or not

```by
def apply(fn: int.() -> None) -> None:
    fn(1)

apply:
    unrelated = "block"
    reveal_type(unrelated)  # revealed: "block"
```

### a bare assignment writes the receiver's member

a bare assignment declares nothing, so it does not take the name — it writes the member, and every
mention of the name in the block goes on meaning that member

```by
class Tag:
    var href: str

    def __init__(self) -> None:
        self.href = ""

def apply(fn: Tag.() -> None) -> None: ...

apply:
    href = "/x"
    reveal_type(href)  # revealed: str
```

the write is checked against the member, exactly as `self.href = …` is

```by
class Tag:
    var href: str

    def __init__(self) -> None:
        self.href = ""

def apply(fn: Tag.() -> None) -> None: ...

apply:
    # error: [invalid-assignment]
    href = 123
```

### a call the receiver cannot take reaches past it

a name used as a callee only claims the receiver's member when that member accepts the call. the
walk is by the call's *shape* — how many positional arguments, and which keywords — so a call the
receiver's member cannot bind carries on outwards to whatever else declares the name

```by
class Repeater:
    def emit(self, times: int) -> int:
        return times

def apply(fn: Repeater.() -> None) -> None: ...

def emit(label: str, times: int) -> str:
    return label

apply:
    reveal_type(emit(2))  # revealed: int
    reveal_type(emit("a", 2))  # revealed: str
```

### a call nothing else can take stays with the receiver

no level of the tower has an applicable candidate, so the nearest one is used and the call reports
its own mismatch rather than an unresolved name

```by
class Repeater:
    def emit(self, times: int) -> None: ...

def apply(fn: Repeater.() -> None) -> None: ...

apply:
    emit(1, 2)  # error: [too-many-positional-arguments]
```

## an unknown member

a member the receiver does not have is still an unresolved reference

```by
def apply(fn: int.() -> None) -> None:
    fn(1)

apply:
    nonesuch  # error: [unresolved-reference]
```

## a name no member answers for is an ordinary local

```by
class Tag:
    var href: str

    def __init__(self) -> None:
        self.href = ""

def apply(fn: Tag.() -> None) -> None: ...

apply:
    unrelated = "/x"
    reveal_type(unrelated)  # revealed: "/x"
```

## the member wins over a binding outside the block

A block writes through to the scope around it, but the receiver outranks that scope — for reads and
for writes alike, so both sides of the `=` mean the same thing. An outer binding of the same name is
not reachable from the block.

```by
class Tag:
    var href: str

    def __init__(self) -> None:
        self.href = ""

def apply(fn: Tag.() -> None) -> None: ...

href = "outer"

apply:
    href = "/x"
    reveal_type(href)  # revealed: str
```

## a plain callback block has no implicit members

```by
def apply(fn: (int) -> None) -> None:
    fn(1)

apply:
    imag  # error: [unresolved-reference]
```

## gradual receiver callable

`T.(...) -> R` keeps the receiver, with the rest of the parameter list gradual

```by
def apply(fn: int.(...) -> str) -> None:
    receiver = 1
    reveal_type(receiver.fn("anything", keyword=1))  # revealed: str
```

## nesting

a receiver callable nests like any other type expression

```by
from typing import Callable

a: list[int.() -> str] = []
reveal_type(a)  # revealed: list[(int, /) -> str]

def optional(b: int.() -> str | None) -> None:
    reveal_type(b)  # revealed: ((int, /) -> str) | None

def curried(fn: int.() -> str.() -> bytes) -> None:
    reveal_type(fn)  # revealed: (int, /) -> ((str, /) -> bytes)
```

## inline protocols

a receiver callable composes with an [inline protocol](basedpython_inline_protocol.md) in either
direction — as a protocol member's type, and as the receiver itself

```by
def data_member(p: protocol(fn: int.() -> str)) -> None:
    reveal_type(p.fn)  # revealed: (int, /) -> str

def method_member(p: protocol(def f(self, cb: int.() -> str) -> None)) -> None:
    reveal_type(p.f)  # revealed: (cb: (int, /) -> str) -> None

def receiver_of_protocol(fn: protocol(a: int).() -> None) -> None:
    reveal_type(fn)  # revealed: (protocol(a: int), /) -> None
```

## names that resolve with no binding behind them

the basedpython names that resolve with nothing bound behind them — `Character`, `Some`, the
implicitly available `typing` spellings — sit outside the block like any other name, so a receiver
member of that spelling wins inside the block

```by
class Lexer:
    Character: int = 1
    digit: str = "0"

def apply(fn: Lexer.() -> None) -> None: ...

apply:
    reveal_type(digit)  # revealed: str
    reveal_type(Character)  # revealed: int
```

## `.py` files reject the syntax

```py
def apply(fn: int.() -> str) -> None:  # error: [invalid-syntax]
    pass
```
