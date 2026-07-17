# unpack syntax

basedpython polyfills the PEP 646 starred-type syntax for python versions below 3.11:

```by
def f(*args: *tuple[int, ...]):
    pass
```

transpiles to:

```python
from typing_extensions import Unpack
def f(*args: Unpack[tuple[int, ...]]):
    pass
```

the starred form (`*T`) is native in python 3.11+. for earlier targets the equivalent `Unpack[T]` form is used instead. the import resolves to `typing_extensions` because `typing.Unpack` only exists from 3.11 — the version at which this transform stops firing

## when it applies

the transform rewrites starred types in variadic parameter annotations and inside subscript slices:

```by
def f(*args: *tuple[int, ...]): ...
coords: tuple[*Ts]

class Stack(Generic[*Ts]): ...
```

starred expressions in value positions (unpacking in assignments, function calls, etc.) are never affected

## `--min-version` interaction

when targeting Python 3.11 or later, the starred form is valid natively and the transform is a no-op
