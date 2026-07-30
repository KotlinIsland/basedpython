# basedpython: precise unsolved type variables

a call can leave a type variable entirely unsolved, because no argument constrains it. `Never` is
the precise answer: no value ever reaches that position, so nothing the call returns can be observed
at that type. python's gradual guarantee asks for `Unknown` instead, which silences every error that
would follow from the call site.

`analysis.precise-unsolved-typevars` is on by default and controls this. it applies to plain python
files too.

## an unsolved type variable

```toml
[environment]
python-version = "3.13"
```

```py
def f[T]() -> T:
    raise NotImplementedError

a = f()
reveal_type(a)  # revealed: Never
```

the legacy spelling says the same thing:

```py
from typing import TypeVar

T = TypeVar("T")

def g() -> T:
    raise NotImplementedError

reveal_type(g())  # revealed: Never
```

a type variable an argument *does* solve is unaffected:

```py
def identity[T](x: T) -> T:
    return x

reveal_type(identity(1))  # revealed: Literal[1]
```

## constructors

```toml
[environment]
python-version = "3.13"
```

```py
class Box[T]:
    def __init__(self, *values: T) -> None: ...

reveal_type(Box())  # revealed: Box[Never]
reveal_type(Box(1))  # revealed: Box[Literal[1]]
```

a pep 696 default takes priority over `Never`:

```py
class Defaulted[T = str]:
    def __init__(self, *values: T) -> None: ...

reveal_type(Defaulted())  # revealed: Defaulted[str]
```

## only where the type variable is an output

```toml
[environment]
python-version = "3.13"
```

`Never` describes a value that cannot be observed. Where the type variable is also written through
or passed back in, the same substitution would instead say that nothing can ever be put there, so an
invariant or contravariant occurrence keeps the gradual `Unknown`.

an invariant occurrence — the element of a `list`, the key of a `dict`:

```py
def build[T](key: T | None) -> dict[T, int]:
    raise NotImplementedError

reveal_type(build(None))  # revealed: dict[Unknown, int]
```

a contravariant occurrence — the parameter of a returned callable:

```py
from typing import Callable

def pipe[A, B](f: Callable[[A], B]) -> Callable[[A], B]:
    return f

def sink[X](x: X) -> None: ...

reveal_type(pipe(sink))  # revealed: (Unknown, /) -> None
```

## the call still returns

```toml
[environment]
python-version = "3.13"
```

a return type of `Never` normally says the callee does not return, and a statement-level call to it
ends the flow. an unsolved type variable says nothing about control flow, so the code after such a
call stays reachable:

```py
def f[T]() -> T:
    raise NotImplementedError

def _() -> None:
    x = 1
    f()
    reveal_type(x)  # revealed: Literal[1]
```

a callee that genuinely does not return is unaffected, including through a generic call that solves
from it:

```py
from typing import NoReturn, TypeVar

T = TypeVar("T")

def identity(x: T) -> T:
    return x

# no `invalid-return-type`: the call is terminal
def _() -> NoReturn:
    identity(exit())
```

## disabled

```toml
[environment]
python-version = "3.13"

[analysis]
precise-unsolved-typevars = false
```

with the option off, an unsolved type variable falls back to the gradual `Unknown`:

```py
def f[T]() -> T:
    raise NotImplementedError

reveal_type(f())  # revealed: Unknown

class Box[T]:
    def __init__(self, *values: T) -> None: ...

reveal_type(Box())  # revealed: Box[Unknown]
```
