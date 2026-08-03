# keyword-variadic packs

in a `.by` file the PEP-695 spelling `[**Kwargs]` declares a *keyword-variadic pack* — an ordered
mapping of parameter name to type — rather than a `ParamSpec`:

```by
class A[**Kwargs]:
    def get(self) -> (**Kwargs) -> None:
        raise NotImplementedError

def f(a: A[foo=int, bar=str]):
    a.get()(foo=1, bar="x")
```

a pack is a true variadic: its fields are written inline at the specialization site, and unpacking
it with `**` splices them into a parameter list. a `ParamSpec` by contrast is opaque — it can only
be forwarded whole, through `P.args` / `P.kwargs`

## specialization

a pack is specialized by keyword. every keyword type argument is one of its fields:

```by
def f(a: A[foo=int, bar=str]):
    reveal_type(a)  # A[foo=int, bar=str]
```

`A[()]` is the empty pack. field order is source order, and it is significant — `A[foo=int, bar=str]` and `A[bar=str, foo=int]` are different types

## unpacking

`**Kwargs` in a [callable arrow](callable.md) contributes the pack's fields as keyword-only
parameters:

```by
class A[**Kwargs]:
    def get(self) -> (**Kwargs) -> None: ...

def f(a: A[foo=int, bar=str]):
    reveal_type(a.get())  # (*, foo: int, bar: str) -> None
```

a prefix works too — `(int, **Kwargs) -> None` is `(int, /, *, foo: int, bar: str) -> None`

## inference from keyword arguments

`**kwargs: **Kwargs` unpacks the pack into a parameter list. the star count follows the pack's
declaration — `[**Kwargs]` unpacks with `**`, the way `[*Ts]` unpacks with `*` — so a single star
on a pack is a syntax error. a call's keyword arguments then solve the pack, one field each, in
source order:

```by
class A[**Kwargs]:
    init(**kwargs: **Kwargs)

a = A(x=1, y="s")
reveal_type(a)  # final A[x=int, y=str]
```

the pack is solved as a whole rather than per-argument, so `A()` gives the empty pack `A[()]`. field
types are promoted the way any inferred type argument is, so `x=1` yields `int`, not `Literal[1]`

## alongside ordinary type variables

a pack claims every keyword argument, so the other type variables of the same class are given
positionally:

```by
class Two[T, **Kwargs]:
    def get(self) -> (T, **Kwargs) -> None: ...

def f(t: Two[bytes, foo=int]):
    reveal_type(t.get())  # (bytes, /, *, foo: int) -> None
```

this is why a pack's context can't also use the
[keyword-subscript](kw-subscript.md) form for binding typevars by name: with a pack in scope,
`A[foo=int]` always names a field

## scope

the keyword-pack reading is confined to `.by`. `.byi` is the interop surface with python's typing
ecosystem — the vendored typeshed is converted from upstream, where `**P` means `ParamSpec` — so a
PEP-695 `**P` in a stub is still a `ParamSpec`.

in `.by` a `ParamSpec` is a type variable bound by the *top parameters* form — `class A[P: (*: *, **: *)]` — an anonymous variadic and an anonymous keyword-variadic, both admitting anything. every
parameter list is a subtype of it, so the bound ranges over all parameter lists. that is what a
python `ParamSpec` reverse-transpiles to, so `class A[**P]` round-trips. the legacy
`P = ParamSpec("P")` form also works, and the arrow syntax unpacks any of them

## lowering

`class A[**Kwargs]` is a `ParamSpec` at runtime, and python has no keyword subscript, so fields
lower to the `ParamSpec` list form:

```by
def f(a: A[foo=int, bar=str]): ...
```

transpiles to:

```python
def f(a: A[[int, str]]): ...
```

field names are erased, matching python's own erasure of type arguments — the names are checked
against the `.by` source, not the emitted python

`**kwargs: **Kwargs` takes the `ParamSpec` spelling, which is what the pack is at runtime:

```by
def __init__(self, **kwargs: **Kwargs) -> None: ...
```

transpiles to:

```python
def __init__(self, **kwargs: Kwargs.kwargs) -> None: ...
```

## splicing into a dict literal type

`{**Kwargs}` contributes the pack's fields to a [dict literal type](typed-dict-literal.md). the
pack contributes nothing until it is specialized, and it is spliced in after the keys written
inline, so a field it carries wins over a key of the same name:

```by
class A[**Kwargs]:
    init(**args: **Kwargs)
    def get(self) -> {"tag": int, **Kwargs}: ...

a = A(a=1, b="s")
reveal_type(a.get())  # {"a": int, "b": str, "tag": int}
```

before specialization the pack shows as a pending splice — `{"tag": int, **Kwargs@A}`

## limitations

- a declared type does not drive a constructor call's arity. `A[foo=int]()` correctly reports the
    missing `foo` argument, because the pack is already concrete when the parameters are matched;
    `a: A[foo=int] = A()` does not, because the pack is only pinned afterwards, by inference. worse,
    the declared pack is then adopted unchecked, so `a: A[foo=int] = A(bar=1)` is accepted
- a type variable reachable only through a `{**Kwargs}` splice reads as bivariant, so a class whose
    only use of its pack is `{**Kwargs}` does not reject a mismatched specialization. giving the
    pack a variance there is what surfaces the adoption bug above, so the two have to be fixed
    together. spelling the pack in an [arrow](callable.md) as well restores invariance
- a pack cannot yet be *forwarded* as a type argument (`A[**Kwargs]` inside another signature)
- a pack cannot have a default (`class D[**Kwargs = ()]`)
- a generic context containing a pack skips literal promotion for its other type variables, so
    `class Two[T, **Kwargs]` infers `T` as `Literal[...]`. this is pre-existing ty behaviour, shared
    with `ParamSpec`

## see also

- [bounds on a variadic pack](pack-bounds.md) — `**Kwargs: int` and `**Kwargs: **{"a": int}`
- [callable arrow syntax](callable.md) — the `**` unpacking position
- [keyword arguments in subscripts](kw-subscript.md) — binding typevars by name
- [`ParamSpec` / `Concatenate` arrow callables](callable.md)
