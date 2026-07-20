# basedpython: version-gated typing imports

basedpython makes version-gated `typing` (and `warnings`) members available regardless of the target
Python version. When a name postdates the target version, the transpiler rewrites
`from typing import X` to `from typing_extensions import X`; the type checker mirrors that redirect
so an explicit import resolves the same way the emitted code will.

```toml
[environment]
python-version = "3.10"
```

## `reveal_type` imported explicitly

`typing.reveal_type` was added in 3.11, so on a 3.10 target it is resolved from `typing_extensions`.

```by
from typing import reveal_type

reveal_type(1)  # revealed: 1
```

## other 3.11 names

```by
from typing import Self, assert_type, Never, LiteralString

class C:
    def clone(self) -> Self:
        return self

reveal_type(C().clone())  # revealed: C
```

## a split import keeps the non-gated half

A name already available at the target version (`TypeVar`) is resolved from `typing` as usual, while
the gated name falls back to `typing_extensions`.

```by
from typing import TypeVar, Self

T = TypeVar("T")
```

## `warnings.deprecated`

`warnings.deprecated` was added in 3.13; on a 3.10 target it resolves from `typing_extensions`.

```by
from warnings import deprecated

@deprecated("use something else")
def old() -> None: ...
```

## genuinely missing members still error

The fallback only covers the curated set of version-gated names, so a name that exists in neither
`typing` nor `typing_extensions` still reports an unresolved import.

```by
# error: [unresolved-import]
from typing import NotARealTypingMember
```

## non-typing modules are unaffected

The redirect is scoped to `typing` and `warnings`; other modules resolve normally.

```by
# error: [unresolved-import]
from os import definitely_not_a_real_os_member
```
