# basedpython-ui: recognising the framework

`basedpython_ui` is a compose-style ui framework. Its observables — `State[T]`, `StateList[T]`,
`StateDict[K, V]`, `Derived[T]` and `Ambient[T]`, declared in `basedpython_ui.runtime` — and its
`@composable` decorator are recognised by their resolved definitions, so the ui-specific checks can
build on them. The package re-exports everything from its `__init__`, and recognition follows the
re-export. Each section below installs a mock of the package in site-packages.

## the observables are ordinary generic classes

An observable's value keeps the type it was created with, whether the class is named directly or
through the constructor function.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.pyi`:

```pyi
from .runtime import (
    Ambient as Ambient,
    Derived as Derived,
    State as State,
    StateDict as StateDict,
    StateList as StateList,
    state as state,
)
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.pyi`:

```pyi
from collections.abc import Iterator

class State[T]:
    value: T
    def __init__(self, initial: T) -> None: ...

class StateList[T]:
    def __iter__(self) -> Iterator[T]: ...

class StateDict[K, V]:
    def __getitem__(self, key: K) -> V: ...

class Derived[T]:
    value: T

class Ambient[T]:
    default: T

def state[T](initial: T) -> State[T]: ...
```

```by
from basedpython_ui import Ambient, Derived, State, StateDict, StateList, state

let count = state(0)
reveal_type(count)  # revealed: State[int]
reveal_type(count.value)  # revealed: int

let named = State("x")
reveal_type(named.value)  # revealed: str

def show(items: StateList[str], table: StateDict[str, int], total: Derived[float], theme: Ambient[str]):
    for item in items:
        reveal_type(item)  # revealed: str
    reveal_type(table["a"])  # revealed: int
    reveal_type(total.value)  # revealed: float
    reveal_type(theme.default)  # revealed: str
```

## a composable keeps its function type

`@composable` is identity-typed, so a decorated function is still the function it was written as — a
trailing block, a `once` callback and every other function-literal feature keep working through it.
The decorator is recognised through the package re-export and through its declaring module alike.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.pyi`:

```pyi
from .runtime import composable as composable
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.pyi`:

```pyi
def composable[F](fn: F) -> F: ...
```

```by
from basedpython_ui import composable
from basedpython_ui.runtime import composable as declared

@composable
def Counter(label: str, once content: () -> None) -> None:
    content()

reveal_type(Counter)  # revealed: def Counter(label: str, content: () -> None)

@composable
def App():
    Counter("clicks"):
        pass

@declared
def Row() -> None: ...

reveal_type(Row)  # revealed: def Row()

```
