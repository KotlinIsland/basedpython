# The `collections.abc` ABCs

`Mapping`, `Iterator`, `Sequence` and the rest of the abstract base classes for containers are
defined in `_collections_abc`, which is where python defines them: `collections.abc` is
`from _collections_abc import *`, and `typing` re-exports the same classes under its deprecated
aliases. Whichever of the three a program imports, it gets the one class.

## One class behind three spellings

```py
import collections.abc
import typing
from ty_extensions import static_assert
from ty_extensions._internal import is_equivalent_to

static_assert(is_equivalent_to(typing.Mapping[str, int], collections.abc.Mapping[str, int]))

def f(x: typing.Iterator[int], y: collections.abc.Iterator[int]) -> None:
    a: collections.abc.Iterator[int] = x
    b: typing.Iterator[int] = y
```

## The public module is the one a diagnostic names

The classes physically live in the private `_collections_abc`, but nothing writes that name, so
nothing shows it either.

`shadow.py`:

```py
from typing import Protocol

class Iterator(Protocol):
    def __nexxt__(self) -> str: ...

def shadowed() -> Iterator:
    raise NotImplementedError
```

```py
import collections.abc
import shadow

def f() -> collections.abc.Iterator[str]:
    # error: [invalid-return-type] "Return type does not match returned value: expected `collections.abc.Iterator[str]`, found `shadow.Iterator`"
    return shadow.shadowed()
```

## `collections.abc.Set` is `typing.AbstractSet`

The runtime calls this class `Set`, but `typing` cannot: it needs that name for the deprecated alias
of `builtins.set`. Both spellings reach the same class.

```py
import collections.abc
import typing
from ty_extensions import static_assert
from ty_extensions._internal import is_equivalent_to

static_assert(is_equivalent_to(collections.abc.Set[int], typing.AbstractSet[int]))

def f(x: collections.abc.Set[int]) -> None:
    y: typing.AbstractSet[int] = x

def g(x: typing.Set[int]) -> None:
    reveal_type(x)  # revealed: set[int]
```

## `Callable`

`Callable` has a real class definition alongside the other ABCs, rather than typeshed's
`Callable: _SpecialForm`. A subscripted `Callable` still describes a callable type, from any of the
three modules that export it.

```py
import collections.abc
import typing
from ty_extensions import static_assert
from ty_extensions._internal import is_equivalent_to

static_assert(is_equivalent_to(typing.Callable[[int], str], collections.abc.Callable[[int], str]))

def f(x: collections.abc.Callable[[int], str], y: typing.Callable[..., int]) -> None:
    reveal_type(x)  # revealed: (int, /) -> str
    reveal_type(x(1))  # revealed: str
    reveal_type(y)  # revealed: (...) -> int
```

A function is assignable to it, and it is still a valid `isinstance` target.

```py
from collections.abc import Callable

def takes(f: Callable[[int], str]) -> str:
    return f(1)

def stringify(n: int) -> str:
    return str(n)

takes(stringify)

def check(x: object) -> None:
    if isinstance(x, Callable):
        reveal_type(x)  # revealed: (...) -> Unknown
```

## Implicitly available in basedpython

basedpython makes these names available in a type position with no import; the bare name resolves to
the same class, and the transpiler emits the matching `from typing import …`.

```by
import collections.abc
from ty_extensions import static_assert
from ty_extensions._internal import is_equivalent_to

static_assert(is_equivalent_to(Mapping[str, int], collections.abc.Mapping[str, int]))

def f(x: Sequence[int], call: Callable[[int], str]) -> str:
    return call(x[0])
```
