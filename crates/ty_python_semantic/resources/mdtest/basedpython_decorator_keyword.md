# basedpython: `decorator def`

basedpython exposes a `decorator` soft keyword on `def`. it declares a function whose first
positional parameter is the decorated callable and whose remaining parameters are keyword-only
options. the transpile expands the declaration into two `@overload` stubs plus a runtime dispatcher

```toml
[environment]
python-version = "3.12"
```

## body type-checks against declared parameters

```by
from typing import Callable

decorator def d(fn: Callable[..., object], option: bool = False) -> int:
    reveal_type(fn)  # revealed: (...) -> object
    reveal_type(option)  # revealed: bool
    return 1 if option else len(str(fn))
```

## direct decoration applies the declared return type

`@d` applied directly to a function calls `d(fn)`, which returns the function's declared return
type. the synthetic decorator that drives the transpile is invisible to ty — function `d` appears
with its original signature

```by
from typing import Callable

decorator def d(fn: Callable[..., object], option: bool = False) -> int:
    return 1 if option else len(str(fn))

def g() -> str:
    return "x"

reveal_type(d(g))  # revealed: int
reveal_type(d(g, option=True))  # revealed: int
```

## no options — single positional parameter

```by
from typing import Callable

decorator def trace(fn: Callable[..., object]) -> int:
    return len(str(fn))

def h() -> None: ...

reveal_type(trace(h))  # revealed: int
```

## the decorated function's unannotated parameters take their declared types

a decoration hands the function to the decorator, so the callable the declaration says it accepts is
type context for the decorated function's parameters. this is the same context that types the
parameters of a lambda passed to `d` directly, and it is not particular to `decorator def` — see
`bidirectional.md`

```by
decorator def d(fn: (int) -> None)

@d
def f(i):
    reveal_type(i)  # revealed: int
```

## multiple options

```by
from typing import Callable

decorator def configure(
    fn: Callable[..., object],
    name: str = "default",
    count: int = 0,
) -> str:
    return name * count

def target() -> None: ...

reveal_type(configure(target))  # revealed: str
reveal_type(configure(target, name="hello", count=2))  # revealed: str
```
