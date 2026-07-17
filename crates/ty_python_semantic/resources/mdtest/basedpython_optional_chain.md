# basedpython: `?.` optional chaining

`a?.b` short-circuits to `None` when `a is None`, otherwise evaluates `a.b`. the result type is the
attribute type unioned with `None`.

a `?.` opens a chain that runs out through every trailer that follows it — `.attr`, `(...)` and
`[...]` — because that is exactly how far the `None if a is None else <rest of chain>` lowering
short-circuits. the `None` therefore belongs to the *last* link of the chain, not to each link: in
`a?.b.c`, an absent `a` skips `.c` too, so the rest of the chain is resolved against `b`'s present
type and only the end result is unioned with `None`.

```toml
[environment]
python-version = "3.12"
```

## simple attribute chain

```by
class C:
    name: str

def f(c: C | None) -> None:
    result = c?.name
    reveal_type(result)  # revealed: str | None
```

## chain continues through a plain attribute

```by
class Address:
    zip: str

class C:
    address: Address

def f(c: C | None) -> None:
    reveal_type(c?.address)       # revealed: Address | None
    reveal_type(c?.address.zip)   # revealed: str | None
```

## chain continues through a method call

```by
class C:
    def greet(self) -> str:
        return "hi"

def f(c: C | None) -> None:
    reveal_type(c?.greet())  # revealed: str | None
```

## chain continues through a subscript

```by
class C:
    items: list[str]

def f(c: C | None) -> None:
    reveal_type(c?.items[0])  # revealed: str | None
```

## chain continues past a call

trailers keep chaining after a call, so the whole tail short-circuits together.

```by
class Inner:
    code: str

    def tag(self) -> int:
        return 1

class C:
    def get(self) -> Inner:
        return Inner()

def f(c: C | None) -> None:
    reveal_type(c?.get().code)    # revealed: str | None
    reveal_type(c?.get().tag())   # revealed: int | None
```

## several `?.` links in one chain

a chain short-circuits once no matter how many of its links are optional, so the result is unioned
with `None` a single time.

```by
class Inner:
    def tag(self) -> int:
        return 1

class Mid:
    inner: Inner | None

class C:
    mid: Mid | None

def f(c: C | None) -> None:
    reveal_type(c?.mid?.inner?.tag())  # revealed: int | None
```

## a `?.` on a receiver that cannot be absent

`?.` only contributes a `None` when its receiver can actually be absent, so a chain over a
non-optional receiver stays non-optional.

```by
class C:
    name: str

def f(c: C) -> None:
    reveal_type(c?.name)  # revealed: str
```

## an optional attribute inside a chain is still an error

the chain's `None` is the receiver's, not the attribute's. an attribute that is optional in its own
right stays optional, so calling or dereferencing it is still reported: `?.` guards `c`, not `cb`.

```by
from typing import Callable

class Inner:
    code: str

class C:
    cb: Callable[[], int] | None
    inner: Inner | None

def f(c: C | None) -> None:
    # error: [call-non-callable]
    c?.cb()
    # error: [unresolved-attribute]
    c?.inner.code
```

## guarding an optional attribute inside a chain

writing a second `?.` guards the attribute's own `None`.

```by
class Inner:
    code: str

class C:
    inner: Inner | None

def f(c: C | None) -> None:
    reveal_type(c?.inner?.code)  # revealed: str | None
```

## composes with `??`

```by
class C:
    name: str

def f(c: C | None) -> str:
    return c?.name ?? "anonymous"
```

## `??` collapses a chain that ends in a call

```by
class C:
    def greet(self) -> str:
        return "hi"

def f(c: C | None) -> None:
    reveal_type(c?.greet() ?? "anonymous")  # revealed: str
```

## chains as call arguments

a chain nested in an argument keeps its own short-circuit, and the argument sees the chain's full
type.

```by
class C:
    def greet(self) -> str:
        return "hi"

def takes_optional(value: str | None) -> None: ...
def takes_str(value: str) -> None: ...

def f(c: C | None) -> None:
    takes_optional(c?.greet())
    # error: [invalid-argument-type]
    takes_str(c?.greet())
```
