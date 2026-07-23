# basedpython: `ParamSpec` / `Concatenate` arrow callables

basedpython spells a `ParamSpec` callable `Callable[P, R]` with the arrow form `(**P) -> R`, and a
`Concatenate` callable `Callable[Concatenate[T1, …, P], R]` with `(T1, …, **P) -> R`.

in a `.by` file the PEP-695 spelling `[**P]` declares a
[keyword-variadic pack](basedpython_keyword_variadic.md), not a `ParamSpec`, so a `ParamSpec` is
declared with the legacy `P = ParamSpec("P")` form. the arrow syntax unpacks either one

```toml
[environment]
python-version = "3.12"
```

## `(**P) -> R` is `Callable[P, R]`

```by
from typing import ParamSpec, Callable

P = ParamSpec("P")

def f(arrow: (**P) -> int, callable: Callable[P, int]):
    reveal_type(arrow)  # revealed: (**P@f) -> int
    # the two forms are the same type — mutually assignable
    arrow = callable
    callable = arrow
```

## `(T1, …, **P) -> R` is `Callable[Concatenate[T1, …, P], R]`

```by
from typing import ParamSpec, Callable, Concatenate

P = ParamSpec("P")

def g(arrow: (str, int, **P) -> bool, callable: Callable[Concatenate[str, int, P], bool]):
    reveal_type(arrow)  # revealed: (str, int, /, *args: P@g.args, **kwargs: P@g.kwargs) -> bool
    arrow = callable
    callable = arrow
```

## a bare `**kwargs: T` is still a kwargs catch-all, not a `ParamSpec`

```by
def h(cb: (**kwargs: str) -> int):
    reveal_type(cb)  # revealed: (**kwargs: str) -> int
```

## PEP-695 `[**P]` stays a `ParamSpec` in a stub

`.byi` is the interop surface with python's typing ecosystem — the vendored typeshed is converted
from upstream, where `**P` means `ParamSpec` — so the keyword-pack reading is confined to `.by`

`stub.byi`:

```byi
from typing import Callable

class C[**P]:
    def get(self) -> Callable[P, int]: ...
```

`main.by`:

```by
from stub import C

def f(c: C[[str, int]]):
    reveal_type(c.get())  # revealed: (str, int, /) -> int
```
