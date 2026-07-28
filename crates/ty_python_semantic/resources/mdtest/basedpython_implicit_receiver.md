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
argument — the one after the receiver

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

### an enclosing `self` keeps its meaning

the receiver is the last fallback, so a method's own `self` still wins — its members are the ones in
scope unqualified either way

```by
def apply(fn: int.() -> None) -> None:
    fn(1)

class C:
    def m(self) -> None:
        apply:
            reveal_type(self)  # revealed: Self@m
            reveal_type(imag)  # revealed: 0
```

## an enclosing binding wins

a name bound anywhere in the lexical chain keeps its ordinary meaning, so a block never captures a
name out from under the scope around it

```by
def apply(fn: int.() -> None) -> None:
    fn(1)

imag: str = "shadow"

apply:
    reveal_type(imag)  # revealed: str
```

## an unknown member

a member the receiver does not have is still an unresolved reference

```by
def apply(fn: int.() -> None) -> None:
    fn(1)

apply:
    nonesuch  # error: [unresolved-reference]
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
    reveal_type(fn)  # revealed: (<Protocol with members 'a'>, /) -> None
```

## `.py` files reject the syntax

```py
def apply(fn: int.() -> str) -> None:  # error: [invalid-syntax]
    pass
```
