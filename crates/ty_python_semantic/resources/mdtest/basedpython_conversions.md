# basedpython conversions

`__from__`, `__into__` and `__of__` describe how a value becomes one of a type. None of them is a
subtyping edge: the checker accepts the value at a *conversion site* — a position where the
transpiler can emit the call — and nowhere else.

## `__from__` on the target converts a call argument

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

def report(t: Fahrenheit) -> None: ...

report(Celsius())
```

## an annotated assignment is a conversion site

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

t: Fahrenheit = Celsius()
reveal_type(t)  # revealed: Fahrenheit
```

## a `return` is a conversion site

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

def make(c: Celsius) -> Fahrenheit:
    return c
```

## an attribute assignment is a conversion site

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

class Reading:
    temperature: Fahrenheit = Fahrenheit()

r = Reading()
r.temperature = Celsius()
```

## `__into__` on the source converts

```by
class Kelvin:
    degrees: float = 0.0

class Celsius:
    degrees: float = 0.0

    def __into__(self) -> Kelvin:
        return Kelvin()

def report(k: Kelvin) -> None: ...

report(Celsius())
```

## a union target converts through the arm that offers it

`x: T? = value` is as much a conversion site as the bare annotation.

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

t: Fahrenheit? = Celsius()
reveal_type(t)  # revealed: Fahrenheit | None
```

## nothing nested inside a generic converts

Converting a `list[Celsius]` would mean an O(n) copy with different identity behind a call, so the
relation stays out of the lattice and only the sites above ask for it.

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

def report_all(ts: list[Fahrenheit]) -> None: ...

cs: list[Celsius] = []
report_all(cs)  # error: [invalid-argument-type]
```

## the repair is single-step

```by
class A:
    x: int = 0

class B:
    x: int = 0

    @classmethod
    def __from__(cls, value: A) -> Self:
        return cls()

class C:
    x: int = 0

    @classmethod
    def __from__(cls, value: B) -> Self:
        return cls()

def takes_c(c: C) -> None: ...

takes_c(A())  # error: [invalid-argument-type]
```

## `__of__` converts a literal

```by
class Meters:
    value: float = 0.0

    @classmethod
    def __of__(cls, value: int | float) -> Self:
        return cls()

d: Meters = 5
reveal_type(d)  # revealed: Meters
```

## `__of__` does not convert a value that is not written out

The brackets — or the digits — have to be in the source, which is what makes wrapping them honest.

```by
class Meters:
    value: float = 0.0

    @classmethod
    def __of__(cls, value: int | float) -> Self:
        return cls()

n = 5
d: Meters = n  # error: [invalid-assignment]
```

## `__of__` converts each element of a literal collection

```by
class Meters:
    value: float = 0.0

    @classmethod
    def __of__(cls, value: int | float) -> Self:
        return cls()

lengths: list[Meters] = [1, 2, 3]
reveal_type(lengths)  # revealed: list[Meters]
```

## a whole-value conversion wins over its elements

`Vec3.__of__` takes the list itself, so the literal converts once rather than element-wise — the
choice never depends on ordering.

```by
class Vec3:
    x: float = 0.0

    @classmethod
    def __of__(cls, value: list[float]) -> Self:
        return cls()

v: Vec3 = [1.0, 2.0, 3.0]
reveal_type(v)  # revealed: Vec3
```

## `__of__` accepts a display whose elements are not literals

```by
class Bag:
    @classmethod
    def __of__(cls, value: list[int]) -> Self:
        return cls()

def compute() -> int:
    return 3

b: Bag = [1, 2, compute()]
reveal_type(b)  # revealed: Bag
```

## a comprehension is not a literal

Its contents come from another collection, which is the line element-wise conversion is drawn on.

```by
class Bag:
    @classmethod
    def __of__(cls, value: list[int]) -> Self:
        return cls()

b: Bag = [x for x in [1, 2, 3]]  # error: [invalid-assignment]
```

## a value that already fits is left alone

A conversion only ever *adds* an assignment that fails without it.

```by
class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: int) -> Self:
        return cls()

def report(t: Fahrenheit) -> None: ...

report(Fahrenheit())
```

## two conversions for one pair are ambiguous

`__from__` and `__into__` are hand-written bodies that can disagree, so which one runs must not
depend on ordering.

```by
class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

class Celsius:
    degrees: float = 0.0

    def __into__(self) -> Fahrenheit:
        return Fahrenheit()

def report(t: Fahrenheit) -> None: ...

report(Celsius())  # error: [ambiguous-conversion]
```

## a `__from__` that is not a classmethod is reported

The lowered call is `Fahrenheit.__from__(value)`, which would bind the value to the first parameter.

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    def __from__(cls, value: Celsius) -> Self:  # error: [invalid-conversion]
        return cls
```

## a malformed `__from__` converts nothing

The declaration is reported at its own site, and the assignment keeps its ordinary error rather than
being repaired by a dunder the lowered call cannot use.

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    def __from__(cls, value: Celsius) -> Self:  # error: [invalid-conversion]
        return cls

t: Fahrenheit = Celsius()  # error: [invalid-assignment]
```

## a `__from__` that does not return its own class is reported

```by
class Other:
    x: int = 0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: int) -> Other:  # error: [invalid-conversion]
        return Other()
```

## an `__into__` that is a classmethod is reported

```by
class Kelvin:
    degrees: float = 0.0

class Celsius:
    degrees: float = 0.0

    @classmethod
    def __into__(cls) -> Kelvin:  # error: [invalid-conversion]
        return Kelvin()
```

## an overloaded `__into__` is reported

`value.__into__()` carries no target, so there would be nothing to dispatch on at runtime.

```by
from typing import overload

class Kelvin:
    degrees: float = 0.0

class Rankine:
    degrees: float = 0.0

class Celsius:
    degrees: float = 0.0

    @overload
    def __into__(self) -> Kelvin: ...  # error: [invalid-conversion]
    @overload
    def __into__(self) -> Rankine: ...
    def __into__(self) -> Kelvin | Rankine:
        return Kelvin()
```

## an overloaded `__from__` is fine

The target dispatches on its argument, and the lowered call is the same either way.

```by
from typing import overload

class Path:
    @overload
    @classmethod
    def __from__(cls, value: str) -> Self: ...
    @overload
    @classmethod
    def __from__(cls, value: bytes) -> Self: ...
    @classmethod
    def __from__(cls, value: str | bytes) -> Self:
        return cls()

def takes_path(p: Path) -> None: ...

def use(s: str, b: bytes) -> None:
    takes_path(s)
    takes_path(b)
```

## conversion dunders travel with the type, not with imports

A conversion is a property of the type it converts to, so it applies wherever that type reaches —
unlike a conformance, which is scoped to the modules that can see it.

`temps.by`:

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

def report(t: Fahrenheit) -> None: ...
```

`main.by`:

```by
from temps import Celsius, report

report(Celsius())
```

## a plain assignment to a declared name is a conversion site

The declared type lives in another statement, and the name carries it here.

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

t: Fahrenheit = Fahrenheit()
t = Celsius()
reveal_type(t)  # revealed: Fahrenheit
```

## an assignment to several names at once is not a conversion site

One value reaches every target, and the targets need not agree about what it should be converted to.

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

t: Fahrenheit = Fahrenheit()
u: Fahrenheit = Fahrenheit()
# error: [invalid-assignment]
# error: [invalid-assignment]
t = u = Celsius()
```

## an unpacking is not a conversion site

Each name is bound an element of the value rather than the value itself.

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

t: Fahrenheit = Fahrenheit()
u: Fahrenheit = Fahrenheit()
# error: [invalid-assignment]
# error: [invalid-assignment]
t, u = (Celsius(), Celsius())
```

## a plain assignment to an undeclared name converts nothing

There is no declared type to convert towards, so the name simply takes the value's own type.

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

t = Fahrenheit()
t = Celsius()
reveal_type(t)  # revealed: final Celsius
```

## a splatted argument is not a conversion site

There is no expression of its own at the call to wrap.

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

def report(t: Fahrenheit) -> None: ...

cs: list[Celsius] = []
report(*cs)  # error: [invalid-argument-type]
```

## conversions do not apply in `.py` files

```py
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> "Fahrenheit":
        return cls()

def report(t: Fahrenheit) -> None: ...

report(Celsius())  # error: [invalid-argument-type]
```

## a union source converts only when every arm offers `__into__`

The lowered `x.__into__()` runs against whichever arm the value actually is, so one arm without it
would be an `AttributeError`.

```by
class Kelvin:
    degrees: float = 0.0

class A:
    def __into__(self) -> Kelvin:
        return Kelvin()

class B:
    def __into__(self) -> Kelvin:
        return Kelvin()

class C:
    degrees: float = 0.0

def report(k: Kelvin) -> None: ...

def both(x: A | B) -> None:
    report(x)

def one_missing(x: A | C) -> None:
    report(x)  # error: [invalid-argument-type]
```

## a `__from__` with the wrong arity is reported

```by
class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius, scale: int) -> Self:  # error: [invalid-conversion]
        return cls()
```

## an `__of__` that takes nothing to convert is reported

```by
class Meters:
    value: float = 0.0

    @classmethod
    def __of__(cls) -> Self:  # error: [invalid-conversion]
        return cls()
```

## an `__into__` that takes parameters is reported

```by
class Kelvin:
    degrees: float = 0.0

class Celsius:
    degrees: float = 0.0

    def __into__(self, offset: int) -> Kelvin:  # error: [invalid-conversion]
        return Kelvin()
```

## an optional value parameter is fine

The lowered call passes exactly one argument, so a default on it changes nothing.

```by
class Meters:
    value: float = 0.0

    @classmethod
    def __of__(cls, value: int = 0) -> Self:
        return cls()

d: Meters = 5
reveal_type(d)  # revealed: Meters
```

## every applicable conversion is named, not just the runner-up

```by
class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls()

    @classmethod
    def __of__(cls, value: Celsius) -> Self:
        return cls()

class Celsius:
    degrees: float = 0.0

    def __into__(self) -> Fahrenheit:
        return Fahrenheit()

def report(t: Fahrenheit) -> None: ...

# `__of__` needs a literal, so only `__from__` and `__into__` apply here
report(Celsius())  # error: [ambiguous-conversion]
```

## `__of__` accepts every literal form

```by
class Anything:
    @classmethod
    def __of__(cls, value: object) -> Self:
        return cls()

a: Anything = None
b: Anything = True
c: Anything = "text"
d: Anything = f"{1}"
e: Anything = b"bytes"
f: Anything = {"k": 1}
g: Anything = {1, 2}
h: Anything = (1, 2)
i: Anything = ...
reveal_type(a)  # revealed: Anything
reveal_type(d)  # revealed: Anything
reveal_type(f)  # revealed: Anything
reveal_type(i)  # revealed: Anything
```

## a `.py` file's method named `__from__` is left alone

`__from__` and friends are ordinary method names in python. Conversions never apply there, so
neither does the validation — inventing an error for them would be a false positive on valid code.

```py
class Widget:
    def __from__(self, other: int) -> None: ...
    def __of__(self, other: int) -> None: ...
    @classmethod
    def __into__(cls) -> int:
        return 1
```
