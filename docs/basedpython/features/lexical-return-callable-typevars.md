# lexical return callable type variables

with `analysis.lexical-return-callable-typevars` on, a type variable written on a function belongs
to that function, even when the only place the signature mentions it is the callable the function
returns

```python
from typing import Callable

def remembers_first[T]() -> Callable[[T], T]:
    state: list[T] = []

    def remember(t: T) -> T:
        if state:
            return state[0]
        state.append(t)
        return t

    return remember

remember: Callable[[int], int] = remembers_first()
remember(1) + 1
remember("a")  # error: [invalid-argument-type]
```

mypy, and ty, read that signature differently: they move `T` off `remembers_first` and onto the
callable it returns, so `remembers_first()` is the generic `[T](T) -> T` and every call of it
solves `T` afresh. that accepts `remember(1)` followed by `remember("a") + "b"` — and the second call
returns the `int` the first one stored, so the program fails at runtime

with this option on, basedpython keeps `T` where it is written. one call of `remembers_first` makes one `remember`, with
one solution for `T`. a call that does not solve it gets the gradual `(Unknown) -> Unknown` rather
than a generic callable it has no grounds for

this is off by default, and applies to `.py` files as well as `.by` when enabled

```toml
[analysis]
lexical-return-callable-typevars = true
```

## returning a generic callable

a function that really does return a generic callable — the usual decorator factory — says so with
a protocol whose `__call__` is generic

```python
from typing import Protocol

class Identity(Protocol):
    def __call__[T](self, t: T, /) -> T: ...

def identity_factory() -> Identity:
    def identity[T](t: T) -> T:
        return t

    return identity

identity = identity_factory()
identity(1)    # int
identity("a")  # str
```

## typeshed

typeshed is written for the other reading. `unittest.skip(reason)` is declared to return a
`Callable[[F], F]` that relies on `F` moving onto the returned callable, so a function decorated with
`@unittest.skip("...")` would lose its type under this one

a stub from the standard library — the bundled typeshed, or one set with `environment.typeshed` —
therefore keeps the moved reading. to apply this one there too

```toml
[analysis]
lexical-return-callable-typevars-in-typeshed = true
```
