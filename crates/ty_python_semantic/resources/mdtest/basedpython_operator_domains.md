# Operator domains

python calls an operator dunder with whatever the other operand is. the method either rejects the
operand by returning `NotImplemented`, and python moves on to the other operand's reflected dunder
(or from `__iadd__` to `__add__`), or it runs with it

what a method can tell apart is only what a runtime check can see: the operand's class, not its type
arguments. so the parameter's type with its type arguments taken off — its runtime domain — is the
set of operands the method runs with. an operand outside the domain is rejected, and an operand
inside it that the parameter doesn't accept is a type error with no fallback

```toml
[environment]
python-version = "3.12"
```

## A rejected operand reaches the reflected dunder

```py
class A:
    def __add__(self, other: int) -> int:
        return 1

class B:
    def __radd__(self, other: A) -> str:
        return "b"

reveal_type(A() + B())  # revealed: str
```

## An in-place dunder that rejects falls back to the binary dunder

`__iadd__` rejects a `str`, so python calls `__add__`, and the target is rebound to its result

```py
class Foo:
    def __iadd__(self, other: int) -> int:
        return 1

    def __add__(self, other: str) -> str:
        return "pear"

foo = Foo()
foo += "a"
reveal_type(foo)  # revealed: str

bar = Foo()
bar += 1
reveal_type(bar)  # revealed: int
```

## An in-place dunder runs with an operand inside its domain

`list.__iadd__` takes an `Iterable[T]`. a `list[str]` is an `Iterable`, so the method can't reject
it and extends the list in place. falling back to `list.__add__` would give `xs` a new type while
`ys` still holds the same list

```py
xs = [1, 2, 3]
ys = xs
# error: [unsupported-operator]
xs += ["a"]
```

## A binary dunder runs with an operand inside its domain

`V[int].__add__` can't tell a `V[str]` from a `V[int]`, so it runs with it, and `W.__radd__` is
never reached

```py
class V[T]:
    def __add__(self, other: "V[T]") -> "V[T]":
        return self

    def __radd__(self, other: object) -> str:
        return ""

def f(a: V[int], b: V[str]):
    # error: [unsupported-operator]
    a + b
```

## Overloads reject only what no overload's domain covers

```py
from typing import overload

class C:
    @overload
    def __add__(self, other: int) -> int: ...
    @overload
    def __add__(self, other: list[int]) -> str: ...
    def __add__(self, other: int | list[int]) -> int | str:
        return 1

class R:
    def __radd__(self, other: C) -> bytes:
        return b""

def f(c: C, r: R, strings: list[str]):
    reveal_type(c + 1)  # revealed: int
    reveal_type(c + r)  # revealed: bytes
    # error: [unsupported-operator]
    c + strings
```

## Each possible receiver rejects on its own

a value typed `int | float` is one or the other. when it's an `int`, `int.__add__` rejects the
`float` and python calls `float.__radd__`

```py
def f(x: int | float):
    reveal_type(x + 1.0)  # revealed: float
```

## A comparison that runs with its operand doesn't fall back

```py
class V[T]:
    def __lt__(self, other: "V[T]") -> bool:
        return True

    def __gt__(self, other: object) -> bool:
        return True

def f(a: V[int], b: V[str]):
    # error: [unsupported-operator]
    a < b
```

an `__eq__` that runs with its operand doesn't fall back to identity either

```py
class E[T]:
    def __eq__(self, other: "E[T]") -> bool:  # error: [invalid-method-override]
        return True

def g(a: E[int], b: E[str]):
    # error: [unsupported-operator]
    a == b
```
