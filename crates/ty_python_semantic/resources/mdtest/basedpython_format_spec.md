# format specs

`f"{value:spec}"` is a call — `type(value).__format__(value, "spec")` — so the spec is checked like
any other argument, and its content is checked against the `__format__` that reads it.

```toml
[environment]
python-version = "3.12"
```

## a class with no `__format__` takes only the empty spec

```py
class Point:
    def __repr__(self) -> str:
        return "Point()"

f"{Point()}"  # ok
f"{Point():>10}"  # error: [invalid-format-spec]
```

## `object.__format__` accepts the empty spec

```py
reveal_type(object().__format__(""))  # revealed: str
object().__format__(">10")  # error: [invalid-argument-type]
```

## a custom `__format__` is checked as a call

a class can now say exactly which specs it takes: `object.__format__` accepts only `""`, so an
override that accepts `""` and a few more of its own is a widening, not a Liskov violation.

```py
from typing import Literal

class Temperature:
    def __format__(self, spec: Literal["", "c", "f"], /) -> str:
        return ""

f"{Temperature()}"  # ok
f"{Temperature():c}"  # ok
f"{Temperature():f}"  # ok
f"{Temperature():k}"  # error: [invalid-format-spec]
```

## a custom `__format__` is not held to the mini-language

```py
class Timestamp:
    def __format__(self, spec: str, /) -> str:
        return ""

# `%Y` is nonsense in the mini-language, and exactly right here
f"{Timestamp():%Y-%m-%d}"  # ok
```

## `str` has no presentation type but `s`

```py
f"{'name':s}"  # ok
f"{'name':d}"  # error: [invalid-format-spec]
```

## `str` has no sign, alternate form or zero padding

```py
f"{'name':+}"  # error: [invalid-format-spec]
f"{'name':#}"  # error: [invalid-format-spec]
f"{'name':05}"  # error: [invalid-format-spec]
f"{'name':,}"  # error: [invalid-format-spec]
```

## `str` does take fill, align, width and precision

```py
f"{'name':*^10.3}"  # ok
```

## an integer has no precision

```py
f"{1:.2}"  # error: [invalid-format-spec]
f"{1:.2f}"  # ok — `f` formats the integer as a float
f"{1:10}"  # ok
```

## an integer has the bases, a float does not

```py
f"{1:x}"  # ok
f"{1:#b}"  # ok
f"{1.0:x}"  # error: [invalid-format-spec]
f"{1.0:.2f}"  # ok
```

## `,` groups the decimal presentations only

```py
f"{1:,}"  # ok
f"{1:,x}"  # error: [invalid-format-spec]
f"{1:_x}"  # ok — `_` also groups the power-of-two bases
```

## a conversion formats the resulting `str`

```py
class Point:
    def __repr__(self) -> str:
        return "Point()"

# `!r` produces a `str`, which does take a width
f"{Point()!r:>10}"  # ok
# and which does not take a presentation type of `d`
f"{Point()!r:d}"  # error: [invalid-format-spec]
```

## a conversion does not excuse a value with no rendering

```py
class Point:
    pass

# `!r` asks for the very repr that has nothing to say
f"{Point()!r}"  # error: [implicit-object-repr]
```

## an exactly-constructed value is checked as its class

`A()` is inferred as `final A`, and it is `A` whose `__format__` reads the spec.

```by
class Point:
    def __repr__(self) -> str:
        return "Point()"

def main():
    p = Point()
    _ = f"{p:>10}"  # error: [invalid-format-spec]
```

## a stub outside the vendored typeshed settles nothing

every stub was written against an `object.__format__` that accepted any `str`, so nobody had a
reason to declare one. numpy's `generic` implements the whole mini-language and its stub never
mentions `__format__`, so silence there is not a rejection.

`vendorish.pyi`:

```pyi
class Scalar: ...
```

```py
from vendorish import Scalar

f"{Scalar():f>10}"  # ok — nothing can be concluded from the stub
```

## the vendored typeshed is taken at its word

it is patched, and the tests run over it, so a stdlib class that takes a spec declares `__format__`
there. these genuinely raise `TypeError` at runtime.

```py
f"{[1, 2]:>10}"  # error: [invalid-format-spec]
f"{(1, 2):>10}"  # error: [invalid-format-spec]
```

## `datetime` reads strftime directives

`date`, `time` and `datetime` hand the spec to `strftime`, so the mini-language does not apply and
the directives are checked instead. an unrecognised one does not raise — the platform writes it
through and the output is quietly wrong — so this is the only thing that catches it.

```py
from datetime import date, datetime, time

d = datetime(2001, 2, 3)

f"{d:%Y-%m-%d}"  # ok
f"{d:%H:%M:%S.%f}"  # ok
f"{d:100%% done}"  # ok
f"{d:%-d}"  # ok — not portable, but deliberate often enough to leave alone
f"{date(2001, 2, 3):%F}"  # ok
f"{time(1, 2):%H:%M}"  # ok
```

## an unrenderable strftime directive is reported

```py
from datetime import datetime

d = datetime(2001, 2, 3)

f"{d:%Q}"  # error: [invalid-format-spec]
f"{d:%Y%}"  # error: [invalid-format-spec]
```

## the mini-language does not reach `datetime`

```py
from datetime import datetime

# `>10` is not an alignment here, it is literal text `strftime` writes through
f"{datetime(2001, 2, 3):>10}"  # ok
```

## the empty spec is never reported

`object.__format__` takes `""` and an override can only widen from there, so every type accepts it
by construction. a call that fails on the empty spec is the checker failing to resolve it — a
constrained type variable, a union it cannot see through — rather than the program being wrong. an
override that really does refuse `""` is reported as a bad override instead.

```py
from typing import Generic, TypeVar

T = TypeVar("T", int, float)

class Range(Generic[T]):
    def __init__(self, lo: T | None):
        self.lo = lo

    def describe(self) -> str:
        return f"{self.lo}"  # ok

    def spec(self) -> str:
        return f"{self.lo:zz}"  # error: [invalid-format-spec]
```

## a union is checked against every member

```py
def f(x: int | None) -> str:
    return f"{x:>10}"  # error: [invalid-format-spec]

def g(x: int | float) -> str:
    return f"{x:>10}"  # ok
```

## a spec built at runtime is not checked

```py
def f(width: int):
    # the spec is only known when it runs
    return f"{1.5:{width}.2f}"
```

## a value of unknown type is not checked

```py
from typing import Any

def f(a: Any):
    return f"{a:whatever}"
```

## the empty spec is always fine

```py
class Point:
    def __repr__(self) -> str:
        return "Point()"

def f(a: Point, b: int, c: str):
    return f"{a}{b}{c}"
```
