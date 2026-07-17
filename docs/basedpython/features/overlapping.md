# overlapping

`Overlapping[T]` is a "safe variance" escape hatch for input positions, imported
from `ty_extensions`. it lets a [covariant](variance.md) (`out T`) class declare a
method that *consumes* `T` without giving up covariance:

```by
def f(xs: list[int]):
    1 in xs         # ok
    object() in xs  # ok
    "a" in xs       # error — str can never be an int
```

it is the loose sibling of [`SafeVariance`](safe-variance.md): both are two-faced
(real type at the call site, bound inside the body); they differ only in the
call-site relation

## the two faces

```by
from ty_extensions import Overlapping

class Mapping[out Key, out Value]:
    def __contains__(self, key: Overlapping[Key]) -> bool:
        reveal_type(key)  # object — the upper bound of Key
        return True

def f(m: Mapping[int, object]):
    1 in m         # ok — int overlaps int
    object() in m  # ok — object overlaps int (it could be an int)
    "a" in m       # error — str is disjoint from int
```

- **at the call site**, an argument is accepted iff it is *not disjoint from* the
    specialized `Key` — i.e. their types overlap (`Overlapping[T]` means exactly
    "not disjoint from `T`"). so a provably-unrelated argument like `"a"` is
    rejected, but a could-be-a-`Key` argument like `object()` is allowed. this is
    looser than a plain `Key` parameter (which would reject `object()`) and
    stricter than `object` (which would accept `"a"`)
- **inside the body**, the parameter is seen as the upper bound of `Key`, so the
    consumed value can never be written back into `Key`-typed covariant storage.
    that erasure is the soundness guard: the value flows *in* at full precision
    and is immediately erased to the bound, so it can never flow back out
    mislabelled

## the membership use case

the canonical use is the `__contains__` method of a covariant container, as in
the example above. a membership test only makes sense for a value that *could* be
an element, so basedpython's typeshed types `Container.__contains__` as
`Overlapping[Element]`

`Container.__contains__` is the abstract requirement, and every container's
`__contains__` takes `Overlapping[Element]` for its own element, so the check
applies uniformly to `list`, `set`, `dict` keys, `tuple`, and any user class
deriving from `Container`/`Collection`. `dict.__getitem__` and `dict.get` (and
`Mapping.__getitem__`/`get`) consume the covariant key the same way, so a lookup
`d[k]` accepts exactly the keys a membership test `k in d` would

because `Overlapping[Key]` relates as `Key` for subtyping and override
compatibility (only the *call site* applies the overlap admissibility check), a
subclass can override such a method with the bare `Key` (or its upper bound)
without a Liskov violation

a membership test whose operands can never overlap is a definite bug, so `in` and
`not in` report `unsupported-operator` when the value is provably disjoint from
the container's element — including narrowing residuals and empty containers.
this is stricter than mainstream checkers (mypy gates the same check behind
`--strict-equality`); in basedpython it is always on

## overlap is exact

`Overlapping` uses the type system's disjointness relation, so it is precise
about literals and unions:

```by
def f(b: list[bool], keys: dict[int | str, object]):
    True in b      # ok — bool overlaps bool
    1 in b         # error — Literal[1] is not a bool
    b"x" in keys   # error — bytes is disjoint from int | str
```

a union is accepted whenever *any* member overlaps, matching the whole-operand
behaviour of a membership test — `x: str | None` may be tested against a
`dict[str, int]` because its `str` part overlaps the key

## `Overlapping[T]` is a parameter annotation

`Overlapping[T]` is only meaningful as a parameter annotation. inside the body
and everywhere else it behaves as the upper bound of `T`. in basedpython source
and in the vendored typeshed it resolves without an import; user code in plain
Python imports it from `ty_extensions`
