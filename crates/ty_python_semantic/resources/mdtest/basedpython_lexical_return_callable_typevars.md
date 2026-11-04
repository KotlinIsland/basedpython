# basedpython: lexical return callable type variables

mypy, and ty, move a type variable that a function's signature only mentions in the callable it
returns onto that callable, reading `def f[T]() -> Callable[[T], T]` as returning the generic
`[T](T) -> T`. that reading is unsound as soon as the body ties the returned callable to a single
solution.

`analysis.lexical-return-callable-typevars` keeps the type variable on the function, as it is
written. it is off by default, and applies to `.py` and `.by` files alike.

## off by default

```toml
[environment]
python-version = "3.13"
```

by default, the type variable moves onto the returned callable, and the unsound call is accepted:

```py
from typing import Callable

def remembers_first[T]() -> Callable[[T], T]:
    raise NotImplementedError

reveal_type(remembers_first)  # revealed: def remembers_first() -> ((T'return, /) -> T'return)

remember = remembers_first()
reveal_type(remember)  # revealed: [T'return](T'return, /) -> T'return
reveal_type(remember("a"))  # revealed: Literal["a"]
```

## the type variable stays on the function

```toml
[environment]
python-version = "3.13"

[analysis]
lexical-return-callable-typevars = true
```

```py
from typing import Callable

def remembers_first[T]() -> Callable[[T], T]:
    state: list[T] = []

    def remember(t: T) -> T:
        if state:
            return state[0]
        state.append(t)
        return t

    return remember

reveal_type(remembers_first)  # revealed: def remembers_first[T]() -> ((T, /) -> T)
```

every call of one `remember` shares one `state`, so the second call below returns the `int` the
first one stored. once the call solves `T`, the mismatch is reported:

```py
remember: Callable[[int], int] = remembers_first()
remember(1) + 1

# error: [invalid-argument-type]
remember("a")
```

a call that leaves `T` unsolved gets the gradual answer, rather than a generic callable it has no
grounds for. `T` is both a parameter and a return type there, so it is not solved to `Never` either:

```py
unsolved = remembers_first()
reveal_type(unsolved)  # revealed: (Unknown, /) -> Unknown
```

the legacy spelling says the same thing:

```py
from typing import TypeVar

U = TypeVar("U")

def legacy_remembers_first() -> Callable[[U], U]:
    raise NotImplementedError

reveal_type(legacy_remembers_first)  # revealed: def legacy_remembers_first[U]() -> ((U, /) -> U)
```

## basedpython syntax

```toml
[environment]
python-version = "3.13"

[analysis]
lexical-return-callable-typevars = true
```

```by
def remembers_first[T]() -> (T) -> T:
    state: list[T] = []

    def remember(t: T) -> T:
        if state:
            return state[0]
        state.append(t)
        return t

    return remember

reveal_type(remembers_first)  # revealed: def remembers_first[T]() -> ((T, /) -> T)

remember: (int) -> int = remembers_first()
# error: [invalid-argument-type]
remember("a")
```

## returning a generic callable

a function that really does return a generic callable says so with a protocol whose `__call__` is
generic:

```toml
[environment]
python-version = "3.13"

[analysis]
lexical-return-callable-typevars = true
```

```py
from typing import Protocol

class Identity(Protocol):
    def __call__[T](self, t: T, /) -> T: ...

def identity_factory() -> Identity:
    def identity[T](t: T) -> T:
        return t

    return identity

identity = identity_factory()
reveal_type(identity(1))  # revealed: Literal[1]
reveal_type(identity("a"))  # revealed: Literal["a"]
```

## typeshed keeps the moved reading

typeshed is written for the reading that moves the type variable. `unittest.skip(reason)` is
declared to return a `Callable[[F], F]` it relies on being generic, so a stub from the standard
library keeps that reading even with the option on:

```toml
[environment]
python-version = "3.13"

[analysis]
lexical-return-callable-typevars = true
```

```py
import unittest

@unittest.skip("not yet")
def skipped(x: int) -> str:
    return ""

reveal_type(skipped)  # revealed: def skipped(x: int) -> str
```

## typeshed opted in

`analysis.lexical-return-callable-typevars-in-typeshed` applies the lexical reading to the standard
library's stubs too:

```toml
[environment]
python-version = "3.13"

[analysis]
lexical-return-callable-typevars = true
lexical-return-callable-typevars-in-typeshed = true
```

```py
import unittest

# error: [dynamic-function-decorator-return]
@unittest.skip("not yet")
def skipped(x: int) -> str:
    return ""

reveal_type(skipped)  # revealed: Unknown
```
