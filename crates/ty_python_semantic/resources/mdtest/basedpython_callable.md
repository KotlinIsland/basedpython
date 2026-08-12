# basedpython: enhanced callable type syntax

basedpython extends callable type syntax beyond `Callable[[T1, T2], R]`. The surface form
`(...) -> R` accepts named parameters, `/`, `*`, variadic `*: T`, kwargs `**: T`, and any
combination thereof. When a signature is denotable by `typing.Callable`, the transpile lowers to the
standard form. Otherwise, a `typing.Protocol` subclass with a synthesized `__call__` method is
emitted.

```toml
[environment]
python-version = "3.12"
```

## denotable form passes through Callable

```by
from typing import Callable

f: (int, str) -> bool = lambda a, b: True
reveal_type(f)  # revealed: (a: int, b: str) -> True
reveal_type(f(1, "x"))  # revealed: True
```

## gradual `(...)` parameter list

a single bare ellipsis parameter list is the gradual "any arguments" callable, equivalent to
`Callable[..., R]`: it accepts any arguments

```by
def call_it(f: (...) -> int) -> None:
    reveal_type(f)  # revealed: (...) -> int
    reveal_type(f(1, "x", c=3))  # revealed: int
```

## gradual `(...)` in return-type position

```by
def make() -> (...) -> str:
    return lambda: "x"

reveal_type(make())  # revealed: (...) -> str
```

## named parameter — non-denotable

```by
g: (a: int) -> str = lambda a: "x"
reveal_type(g)  # revealed: (a: int) -> "x"
reveal_type(g(1))  # revealed: "x"
reveal_type(g(a=2))  # revealed: "x"
```

## named after positional

```by
h: (int, name: str) -> bool = lambda a, name: True
reveal_type(h)  # revealed: (a: int, name: str) -> True
reveal_type(h(1, "y"))  # revealed: True
reveal_type(h(1, name="z"))  # revealed: True
```

## positional-only marker

```by
p: (int, /, name: str) -> bool = lambda x, name: True
# slash marker is currently dropped from display
reveal_type(p)  # revealed: (x: int, name: str) -> True
reveal_type(p(1, name="ok"))  # revealed: True
```

## variadic with type

```by
v: (*: int) -> int = lambda *a: 0
# lambda body literal narrows the bidirectional return
reveal_type(v(1, 2, 3))  # revealed: 0
```

## named variadic

```by
n: (*args: int) -> int = lambda *args: 0
reveal_type(n(1, 2, 3))  # revealed: 0
```

## kwargs catch-all

```by
kw: (**: str) -> int = lambda **kw: 0
reveal_type(kw(a="x", b="y"))  # revealed: 0
```

## named kwargs

```by
nkw: (**kwargs: str) -> int = lambda **kwargs: 0
reveal_type(nkw(a="x"))  # revealed: 0
```

## full form — every shape

```by
f: (int, /, name: str, *args: bool, **kwargs: int) -> None = (
    lambda x, name, *args, **kwargs: None
)
reveal_type(f(1, name="n"))  # revealed: None
reveal_type(f(1, name="n", extra=10))  # revealed: None
```

## bad arg type errors

```by
g: (a: int) -> str = lambda a: "x"
g("wrong")  # error: [invalid-argument-type]
```

## wrong arg name errors

```by
g: (a: int) -> str = lambda a: "x"
# error: [missing-argument]
# error: [unknown-argument]
g(b=1)
```

## non-denotable function param

```by
def call_me(f: (a: int, b: str) -> bool) -> bool:
    return f(a=1, b="x")

def matches(a: int, b: str) -> bool:
    return True

reveal_type(call_me(matches))  # revealed: bool
```

## return-type position

```by
def make() -> (a: int) -> str:
    return lambda a: "x"

reveal_type(make())  # revealed: (a: int) -> str
reveal_type(make()(1))  # revealed: str
```

## inside subscript

```by
xs: list[(a: int) -> str] = [lambda a: "y"]
reveal_type(xs[0])  # revealed: (a: int) -> str
```

## nested callables

```by
n: (a: int) -> (b: str) -> bool = lambda a: lambda b: True
reveal_type(n(1)("y"))  # revealed: True
```

## structural assignability — function to non-denotable Protocol

```by
def matches(a: int, b: str) -> bool:
    return True

f: (a: int, b: str) -> bool = matches
reveal_type(f(1, "x"))  # revealed: bool
```

## structural compatibility error

```by
def wrong(a: int) -> bool:
    return True

# error: [invalid-assignment]
g: (a: int, b: str) -> bool = wrong
```

## tuple paired with callable

```by
a: (int, name: str) = (1, "x")
b: (int, name: str) -> str = lambda x, name: f"{x}-{name}"
reveal_type(a)  # revealed: (int, name: str)
reveal_type(b(a[0], a.name))  # revealed: f"{int}-{str}"
```

## unpacked `TypeVarTuple` as the whole parameter list

`(*Ts) -> R` stands for the parameters `Ts` expands to, exactly as `Callable[[*Ts], R]` does

```by
def f[*Args](fn: (*Args) -> object): ...

f[int, str](lambda a, b: reveal_type((a, b)))  # revealed: (int, str)
```

## unpacked `TypeVarTuple` after a fixed prefix

```by
def f[*Args](fn: (int, *Args) -> object): ...

f[str, bytes](lambda i, s, b: reveal_type((i, s, b)))  # revealed: (int, str, bytes)
```

## unpacked `TypeVarTuple` as a named variadic's annotation

```by
def f[*Args](fn: (*args: *Args) -> object): ...

f[int, str](lambda a, b: reveal_type((a, b)))  # revealed: (int, str)
```

## `Unpack` in a callable parameter list

```by
from typing import Unpack

def f[*Args](fn: (Unpack[Args]) -> object): ...

f[int, str](lambda a, b: reveal_type((a, b)))  # revealed: (int, str)
```

## unpacked fixed tuple in a callable parameter list

matches `Callable[[Unpack[tuple[int, str]]], object]`, which is also displayed with the tuple left
unexpanded

```by
from typing import Unpack

def f(fn: (Unpack[tuple[int, str]]) -> object): ...

reveal_type(f)  # revealed: def f(fn: (*(int, str)) -> object)
```

## an unpacked `TypeVarTuple` in a stub

this is the form the vendored typeshed uses for `Callable[[Unpack[Ts]], R]`

`stub.byi`:

```byi
def apply[*Args](fn: (*Args) -> object) -> None: ...
```

`main.by`:

```by
from stub import apply

apply[int, str](lambda a, b: reveal_type((a, b)))  # revealed: (int, str)
```

## a non-`TypeVarTuple` after `*` stays an anonymous variadic

`(*: T)` annotates each individual argument, so it must not be read as an unpack even when `T` is a
fixed tuple

```by
def f(fn: (*: tuple[int, str]) -> object): ...

reveal_type(f)  # revealed: def f(fn: (*args: (int, str)) -> object)
```
