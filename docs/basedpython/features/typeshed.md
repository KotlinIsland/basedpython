# typeshed improvements

basedpython vendors typeshed as `.byi` stubs, regenerated from upstream on every
sync and then patched deterministically. some of those patches change what the
stdlib's types *mean*; the rest change how they read. both are listed here —
[typeshed patches](../development/typeshed-patches.md) covers how the machinery
works

## type fixes

what basedpython's stdlib says that upstream's does not

### mapping keys are covariant

upstream declares `Mapping` with an invariant key. basedpython makes it
covariant, so a `Mapping[str, int]` is a `Mapping[object, int]`:

```by
def f(m: Mapping[object, int]): ...

f(dict[str, int](a=1))   # accepted
```

`MutableMapping` keeps an invariant key — `__setitem__` needs it

### `re` capture groups are optional, not `Any`

upstream types every "this group may not have participated" position as
`AnyStr | MaybeNone`, and `MaybeNone` is `Any` — so calling a `str` method on a
group that is `None` at runtime passes silently. basedpython spells the
possibility out:

```by
m = re.match(r"(a)?(b)", s)
m.group(1).upper()   # error: `str | None` has no attribute `upper`
```

this covers `Match.group`, `Match.groups`, `Match.groupdict`,
`Match.__getitem__` and the `split` functions. where the pattern is a literal,
[regex group types](regex-groups.md) reads it and gives something exact instead;
this is the fallback for a pattern the checker cannot see

### membership tests check for overlap

`Container` is covariant in its element, so `in` consumes a covariant type
variable in an input position. upstream gives up and types the parameter
`object`. basedpython types it [`Overlapping[Element]`](overlapping.md) — a
value is accepted if it is not disjoint from the element type:

```by
def f(xs: list[int], o: object):
    1 in xs     # ok
    o in xs     # ok — `object` overlaps `int`
    "a" in xs   # error — `str` and `int` are disjoint
```

`Mapping` and `dict` apply the same treatment to `__getitem__` and `get`, which
consume the covariant key

### a fresh container widens at the call site

an invariant container cannot be assigned to a wider specialization — a caller
holding `list[int | None]` could insert a `None` into your `list[int]`. but a
method returning a *brand new* container (`list.copy`, `list.__add__`, the set
algebra, `dict.copy`, ...) hands back an object the caller solely owns, so
widening it is sound:

```by
a: list[int] = [1]
b: list[int | None] = a.copy()   # ok — nothing else holds the copy

reveal_type(a.copy())            # still `list[int]`
```

it is a `Never`-defaulted type parameter unioned into the return type, so with
no expected type inference is unchanged

### `functools.cache` keeps the wrapped signature

upstream parametrizes `_lru_cache_wrapper` by the return type only, so a cached
function loses its parameter list:

```by
@cache
def f(x: int) -> int: ...

f(1, 2, 3)   # accepted upstream; an error in basedpython
```

basedpython captures the whole callable and recovers the signature through
generic self-binding, with a `__get__` overload so a cached *method* is checked
too — no `ParamSpec` or `Concatenate` spelling needed

### hashable keys are required

the key of `dict` and `frozendict`, and the element of `set` and `frozenset`,
are bounded by `Hashable` — an unhashable key is a type error rather than a
runtime `TypeError`

### more covariance in `builtins`

- `frozendict` is fully covariant — it has no mutators, so there is nothing to
    make it invariant
- the value projection of `type.__dict__` is covariant

### borrowing builtins are marked `local`

the builtins that cannot retain their argument take it as
[`local`](local-lifetimes.md), so the checker knows the value does not escape
the call

### context-manager entry methods are abstract

`AbstractContextManager.__enter__` and `AbstractAsyncContextManager.__aenter__`
return `self`, which the type system cannot spell, so upstream marks them
abstract

### deletions

- **mypy/pyright-only overloads** — typeshed carries overloads whose sole
    purpose is to nudge another checker's inference, and which their own comments
    describe as technically covered by a more general overload. `builtins.getattr`
    is the clearest case. ty does not need them
- **dead symbols** — `builtins.function`, a `@type_check_only` stand-in ty models
    natively as `FunctionType`, and `typing.AwaitableGenerator`, which upstream
    itself marks obsolete

## idiom rewrites

same meaning, written the way a `.by` file writes it

- **pep 695 headers** — every legacy `TypeVar(...)` + `Generic[...]` class
    becomes a pep 695 header with [explicit variance](variance.md) (`out` / `in` /
    `in out`) and readable type-parameter names (`_KT_co` → `Key`, `_T_co` →
    `Element`). this is the bulk of the diff
- **`protocol` keyword** — `class C(Base, Protocol)` → [`protocol C(Base)`](inline-protocol.md)
- **arrow callables** — `Callable[[A, B], R]` → [`(A, B) -> R`](callable.md)
- **bare literals** — `Literal[a, b]` → [`a | b`](literal-types.md)
- **`final` modifier** — a `@final` decorator stacked with others becomes the
    [`final`](modifiers.md) class or def modifier
- **`final` declarations** — `x: Final[T]` → `final x: T`
- **`init` shorthand** — a plain `def __init__(self, ...) -> None` →
    [`init(self, ...)`](init-method.md)
- **redundant `-> None`** — a return annotation that only repeats what a bare
    `def` already means is dropped, since [`None` is what a stub with no
    annotation returns](sound-types.md). one that would change the type if
    deleted — an override, a generator — is kept
- **read-only properties** — a non-computed `@property` → a valueless
    [`let NAME: T`](properties.md), which declares the same thing without the
    descriptor machinery
- **type-alias statements** — a non-generic `X: TypeAlias = V` → `type X = V`
- **private aliases and protocols** — an underscore-prefixed alias or protocol
    that nothing outside its module uses →
    `private type X` / `private protocol X`
- **homogeneous tuples** — `tuple[X, ...]` → [`(*: X)`](tuple-types.md)
- **`dynamic`** — every surviving `Any` → [`dynamic`](dynamic.md)
- **implicit typing imports** — the `from typing import ...` names basedpython
    [provides implicitly](implicit-typing.md) are dropped; runtime helpers stay
- **cleanups** — another checker's suppression comments, leftover `: ...` bodies
    on decorated stubs, stranded private typevars, and stray upstream comments
    about mypy quirks are all removed
