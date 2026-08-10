# implicit typing imports

names from the `typing` standard library are implicitly available in `.by`
source. referencing one inserts a matching `from typing import ...` in the
transpiled output without an explicit import:

```by
a: Sequence[int]
```

transpiles to:

```python
from typing import Sequence
a: Sequence[int]
```

multiple references collapse into a single sorted import line:

```by
def f(x: Optional[int], y: Mapping[str, int]) -> Iterable[int]: ...
```

```python
from typing import Iterable, Mapping, Optional
def f(x: Optional[int], y: Mapping[str, int]) -> Iterable[int]: ...
```

## which names

the implicit set covers `typing` members whose role is to describe a type or
structural protocol. it includes the ABCs (`Sequence`, `Mapping`,
`Iterable`, `Iterator`, `MutableMapping`, `MutableSequence`, …), generic
helpers (`Optional`, `Union`, `Type`, `Annotated`), narrowing
(`TypeGuard`), version-specific names (`Self`, `LiteralString`, `Never`,
`Required`, `NotRequired`, `ReadOnly`), and the `Supports*` /
`AsyncContext*` family. `Callable` is implicit too, though
[callable arrow syntax](callable.md) is the spelling to reach for

names whose role is covered by dedicated basedpython syntax are **not** in
the implicit set — referencing them does not auto-import:

- `Final`, `ClassVar`, `NewType`, `final`, `override` — use [modifiers](modifiers.md)
- `Literal` — see [literal types](literal-types.md)
- `Protocol`, `Generic` — use `protocol class` / [generic class syntax](generics.md)
- `TypeVar`, `ParamSpec`, `TypeVarTuple` — use [generic syntax](generics.md)
- `Unpack` — use [unpack syntax](unpack-syntax.md)
- `NamedTuple` — see [anonymous named tuples](anonymous-named-tuple.md)
- `TypedDict` — see [typed dict literals](typed-dict-literal.md)
- `TypeIs` — see [type narrowing predicates](type-is.md)
- `TypeAlias` — use the `type X = …` statement

runtime helpers (`cast`, `get_type_hints`, `get_args`, `get_origin`,
`overload`, `assert_type`, `reveal_type`, …) are also excluded — they must
be imported explicitly

## interaction with existing imports

if the name is already bound at module scope (`from typing import Sequence`,
`from collections.abc import Sequence`, or a user-level `Sequence = …`),
no implicit import is emitted. existing bindings win

## interaction with `typing_extensions`

implicit names that are too new for the target python (`Self`, `Never`,
`LiteralString`, `Required`, `NotRequired`, `ReadOnly`, …) come from
`typing_extensions` automatically. nothing to configure in `.by` source
