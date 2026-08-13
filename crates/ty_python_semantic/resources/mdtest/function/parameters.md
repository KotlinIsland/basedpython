# Function parameter types

## Basic

Within a function scope, the declared type of each parameter is its annotated type (or Unknown if
not annotated). The initial inferred type is the annotated type of the parameter, if any:

```py
def f(declared: int, unannotated):
    reveal_type(declared)  # revealed: int
    reveal_type(unannotated)  # revealed: unannotated@f
```

basedpython infers a signature for the unannotated parameter rather than leaving it `Unknown`, so
the parameter reads as the type variable that stands for whatever callers pass.

The variadic parameter is a variadic tuple of its annotated type; the variadic-keywords parameter is
a dictionary from strings to its annotated type:

```py
def g(*args: int, **kwargs: int):
    reveal_type(args)  # revealed: tuple[int, ...]
    reveal_type(kwargs)  # revealed: dict[str, int]
```

## Unannotated parameters with defaults

If there is no annotation but there is a default value, the parameter is still inferred, and the
default value's type bounds the inferred type variable:

```py
def f(a="foo", b=0, c=True, d=None):
    reveal_type(a)  # revealed: a@f
    reveal_type(b)  # revealed: b@f
    reveal_type(c)  # revealed: c@f
    reveal_type(d)  # revealed: d@f
```

The body is checked against that bound, so an operation the default value does not support is
rejected:

```py
def g(x=0):
    print(x + 1)

    # error: [unsupported-operator] "Operator `+` is not supported between objects of type `x@g` and `Literal["foo"]`"
    x + "foo"
```

The bound also constrains callers, so an argument wider than the default value's type is rejected at
the call site:

```py
# error: [invalid-argument-type] "Argument to function `g` is incorrect: Argument type `float` does not satisfy `int`, inferred for parameter `x`"
g(1.5)
```

## Parameter kinds

```py
from typing import Literal

def f(a, b: int, c=1, d: int = 2, /, e=3, f: Literal[4] = 4, *args: object, g=5, h: Literal[6] = 6, **kwargs: str):
    reveal_type(a)  # revealed: a@f
    reveal_type(b)  # revealed: int
    reveal_type(c)  # revealed: c@f
    reveal_type(d)  # revealed: int
    reveal_type(e)  # revealed: e@f
    reveal_type(f)  # revealed: Literal[4]
    reveal_type(g)  # revealed: g@f
    reveal_type(h)  # revealed: Literal[6]
    reveal_type(args)  # revealed: tuple[object, ...]
    reveal_type(kwargs)  # revealed: dict[str, str]
```

## Unannotated variadic parameters

...are inferred as tuple of Unknown or dict from string to Unknown.

```py
def g(*args, **kwargs):
    reveal_type(args)  # revealed: tuple[Unknown, ...]
    reveal_type(kwargs)  # revealed: dict[str, Unknown]
```

## Annotation is present but not a fully static type

If there is an annotation, we respect it fully and don't union in the default value type.

```py
from typing import Any

def f(x: Any = 1):
    reveal_type(x)  # revealed: Any
```

## Default value type must be assignable to annotated type

The default value type must be assignable to the annotated type. If not, we emit a diagnostic, and
fall back to inferring the annotated type, ignoring the default value type.

```py
# error: [invalid-parameter-default]
def f(x: int = "foo"):
    reveal_type(x)  # revealed: int

# The check is assignable-to, not subtype-of, so this is fine:
from typing import Any

def g(x: Any = "foo"):
    reveal_type(x)  # revealed: Any
```

## TypedDict defaults use annotation context

```py
from typing import TypedDict

class Foo(TypedDict):
    x: int

def x(a: Foo = {"x": 42}): ...
def y(a: Foo = dict(x=42)): ...
```

## TypedDict defaults still validate keys and value types

```py
from typing import TypedDict

class Foo(TypedDict):
    x: int
    y: int

# error: [missing-typed-dict-key]
def missing_key(a: Foo = {"x": 42}): ...

# error: [invalid-argument-type]
def wrong_type(a: Foo = {"x": "s", "y": 1}): ...

# error: [invalid-key]
def extra_key(a: Foo = {"x": 1, "y": 2, "z": 3}): ...
```

## Stub functions

```toml
[environment]
python-version = "3.12"
```

### In Protocol

```py
from typing import Protocol

class Foo(Protocol):
    def x(self, y: bool = ...): ...
    def y[T](self, y: T = ...) -> T: ...

class GenericFoo[T](Protocol):
    def x(self, y: bool = ...) -> T: ...
```

### In abstract method

```py
from abc import abstractmethod

class Bar:
    @abstractmethod
    def x(self, y: bool = ...): ...
    @abstractmethod
    def y[T](self, y: T = ...) -> T: ...
```

### In function overload

```py
from typing import overload

@overload
def x(y: None = ...) -> None: ...
@overload
def x(y: int) -> str: ...
def x(y: int | None = None) -> str | None: ...
```

### In `if TYPE_CHECKING` blocks

We generally view code in `if TYPE_CHECKING` blocks as having the same semantics and exemptions to
code in stub files:

```py
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    def foo(x: bool = ...): ...  # fine
```
