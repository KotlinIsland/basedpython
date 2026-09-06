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

## a declared variance does not change the constructor answer

```toml
[environment]
python-version = "3.13"
```

declaring `in out` pins the subtyping relation between a class's specializations. it says nothing
about what an instance built with no arguments holds, which is nothing, so the type parameter is
still left unsolved as `Never` — the same answer `[]` gets, for the same reason

```by
class Cell[in out T]:
    def __init__(self, *values: T):
        self.values = values

    def add(self, value: T):
        self.values += (value,)

reveal_type(Cell())  # revealed: final Cell[Never]
reveal_type(Cell(1))  # revealed: final Cell[int]
```

a built-in container built by calling its class answers the same way

```by
reveal_type(set())  # revealed: final set[Never]
reveal_type(list())  # revealed: final list[Never]
```

## a type variable an argument reached stays gradual

```toml
[environment]
python-version = "3.13"
```

`Never` says the call built something with nothing in it, which is only true of a type parameter no
argument could have constrained. Where an argument did reach one and the solve still came back
empty, inference gave up rather than the value being empty, and `Never` would move the resulting
error away from the call that could not infer it.

it is the parameter an argument was matched to that decides this, not what the solve produced. `V`
is reached and `K` is not, so a call that fills `value` still leaves `K` uninhabited:

```py
class Keyed[K, V]:
    def __init__(self, value: V): ...

reveal_type(Keyed(1))  # revealed: Keyed[Never, Literal[1]]
```

`map`'s constructor reaches its element type through the callback. the solve over an overloaded
callback and a gradual iterable does not converge, and the fallback stays gradual:

```py
import operator
from typing import Any

ints: list[int] = []
dynamic: Any = []

reveal_type(map(operator.add, ints, dynamic))  # revealed: map[Unknown]
```

## a gradual parameter reaches everything

```toml
[environment]
python-version = "3.13"
```

a parameter annotated `Any` swallows its argument and says nothing about where the argument went, so
a call that filled one has not established that anything is empty.

this is what `dict` and every subclass of it inherit: `__new__(cls, /, *args: Any, **kwargs: Any)`
takes the constructor's arguments before `__init__` does. reading that as "no argument reached the
value type" would make every `defaultdict(list)` hold `Never`, and its values unusable.

```py
from collections import defaultdict

# the key type really is unreached, and `Never` is the right answer for it. the value type is not:
# `default_factory` names it, and the solve simply did not resolve it
reveal_type(defaultdict(list))  # revealed: defaultdict[Never, Unknown]

d = defaultdict(list)
reveal_type(d["key"])  # revealed: Unknown
```

a class of our own with the same catch-all `__new__` answers the same way, and one without it is
unaffected either way:

```py
from typing import Any, Callable

class Caught[K, V]:
    values: list[V]

    def __new__(cls, /, *args: Any, **kwargs: Any) -> "Caught[K, V]":
        raise NotImplementedError

    def __init__(self, make: Callable[[], V] | None, /) -> None: ...

class Plain[K, V]:
    values: list[V]

    def __init__(self, make: Callable[[], V] | None, /) -> None: ...

reveal_type(Caught(list))  # revealed: Caught[Never, Unknown]
reveal_type(Plain(list))  # revealed: Plain[Never, Unknown]
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
