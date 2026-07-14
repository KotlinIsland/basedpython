# basedpython: `ParamSpec` / `Concatenate` arrow callables

basedpython spells a `ParamSpec` callable `Callable[P, R]` with the arrow form `(**P) -> R`, and a
`Concatenate` callable `Callable[Concatenate[T1, …, P], R]` with `(T1, …, **P) -> R`.

```toml
[environment]
python-version = "3.12"
```

## `(**P) -> R` is `Callable[P, R]`

```by
from typing import ParamSpec, Callable
P = ParamSpec("P")

def f[**P](arrow: (**P) -> int, callable: Callable[P, int]):
    reveal_type(arrow)  # revealed: (**P@f) -> int
    # the two forms are the same type — mutually assignable
    arrow = callable
    callable = arrow
```

## `(T1, …, **P) -> R` is `Callable[Concatenate[T1, …, P], R]`

```by
from typing import ParamSpec, Callable, Concatenate
P = ParamSpec("P")

def g[**P](arrow: (str, int, **P) -> bool, callable: Callable[Concatenate[str, int, P], bool]):
    reveal_type(arrow)  # revealed: (str, int, /, *args: P@g.args, **kwargs: P@g.kwargs) -> bool
    arrow = callable
    callable = arrow
```

## a bare `**kwargs: T` is still a kwargs catch-all, not a `ParamSpec`

```by
def h(cb: (**kwargs: str) -> int):
    reveal_type(cb)  # revealed: (**kwargs: str) -> int
```
