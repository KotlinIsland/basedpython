# basedpython: `ParamSpec` / `Concatenate` arrow callables

basedpython spells a `ParamSpec` callable `Callable[P, R]` with the arrow form `(**P) -> R`, and a
`Concatenate` callable `Callable[Concatenate[T1, …, P], R]` with `(T1, …, **P) -> R`.

in a `.by` file the PEP-695 spelling `[**P]` declares a
[keyword-variadic pack](basedpython_keyword_variadic.md), not a `ParamSpec`. a `ParamSpec` is a type
variable bound by the *top parameters* form `(*: *, **: *)` — the parameter list every other
parameter list is a subtype of — or the legacy `P = ParamSpec("P")`. the arrow syntax unpacks any of
them

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

## a top-parameters bound declares a `ParamSpec`

```by
class A[P: (*: *, **: *)]:
    def get(self) -> (**P) -> None:
        raise NotImplementedError

def f(a: A[(int, str)]):
    reveal_type(a)  # revealed: A[(int, str, /)]
    reveal_type(a.get())  # revealed: (int, str, /) -> None
    a.get()(1, "x")
    a.get()("wrong", "x")  # error: [invalid-argument-type]
```

this is what a python `ParamSpec` reverse-transpiles to, so `class A[**P]` in a `.py` file
round-trips to `class A[P: (*: *, **: *)]` and back

## a named field keeps its name

specializing with a parameters spec preserves each field's name and the `/` and `*` markers, so the
parameters are the ones written. a bare type has no name to be passed by, hence the implicit `/`
after it

```by
class A[P: (*: *, **: *)]:
    def get(self) -> (**P) -> None:
        raise NotImplementedError

a = A[(int, foo: str)]()
reveal_type(a)  # revealed: final A[(int, /, foo: str)]

def use(a: A[(int, foo: str)]):
    reveal_type(a.get())  # revealed: (int, /, foo: str) -> None
    a.get()(1, "x")
    a.get()(1, foo="x")
    a.get()(1, foo=2)  # error: [invalid-argument-type]
    a.get()(foo="x")  # error: [missing-argument]
```

## the `*` marker makes the fields after it keyword-only

```by
class A[P: (*: *, **: *)]:
    def get(self) -> (**P) -> None:
        raise NotImplementedError

def use(b: A[(int, *, foo: str)]):
    reveal_type(b.get())  # revealed: (int, /, *, foo: str) -> None
    b.get()(1, foo="x")
    # error: [too-many-positional-arguments]
    # error: [missing-argument] "No argument provided for required parameter `foo`"
    b.get()(1, "x")
```

## the `/` marker makes the fields before it positional-only

```by
class A[P: (*: *, **: *)]:
    def get(self) -> (**P) -> None:
        raise NotImplementedError

def use(c: A[(int, /, b: str)]):
    reveal_type(c.get())  # revealed: (int, /, b: str) -> None
    c.get()(1, "x")
    c.get()(1, b="x")
```

## a `/` after a named field does not parse

a parenthesized group opening with `name: type` is taken as an
[anonymous named tuple](anonymous_named_tuple.md), which has no marker syntax. this predates keyword
packs and applies to the callable arrow equally

```by
class A[P: (*: *, **: *)]: ...

# error: [invalid-syntax] "Expected an expression"
# error: [invalid-syntax] "Expected an expression"
d = A[(a: int, /, b: str)]()
```

## variadic fields

```by
class A[P: (*: *, **: *)]:
    def get(self) -> (**P) -> None:
        raise NotImplementedError

def use(d: A[(int, *rest: str, **opts: bytes)]):
    reveal_type(d.get())  # revealed: (int, /, *rest: str, **opts: bytes) -> None
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
