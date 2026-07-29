# basedpython: `float` and `complex` are exact

Python's typing spec special-cases `float` to mean `int | float` and `complex` to mean
`int | float | complex`. basedpython does not — `float` is just `float`, and `complex` is just
`complex`. The transpiler restores Python semantics by rewriting bare `float` / `complex` in type
positions to `ty_extensions.JustFloat` / `ty_extensions.JustComplex`.

## bare `float` annotation in `.by`

`float` in a `.by` annotation rejects `int` values

```by
x: float = 1.0
y: float = 1  # error: [invalid-assignment]
```

## bare `complex` annotation in `.by`

`complex` in a `.by` annotation rejects `int` and `float` values

```by
a: complex = 1j
b: complex = 1.0  # error: [invalid-assignment]
c: complex = 1  # error: [invalid-assignment]
```

## `float` parameter rejects `int` argument

```by
def f(x: float) -> None: ...

f(1.0)
f(1)  # error: [invalid-argument-type]
```

## `float` propagates through generic subscript

```by
def takes(xs: list[float]) -> None: ...

takes([1.0, 2.0])
takes([1, 2])  # error: [invalid-argument-type]
```

## `.py` keeps Python's special-case semantics

A `.py` file uses the typing-spec special case — `float` annotation accepts `int`, `complex` accepts
`int` and `float`.

`mod.py`:

```py
def takes_float(x: float) -> None: ...
def takes_complex(x: complex) -> None: ...

takes_float(1)  # accepted under typing spec
takes_complex(1.0)  # accepted under typing spec
```

## `.py` exporter, `.by` consumer

A function imported from a `.py` file keeps its Python type semantics. The `.py` annotation permits
`int`, even when called from a `.by` file.

`pylib.py`:

```py
def takes_float(x: float) -> None: ...
```

`consumer.by`:

```by
from pylib import takes_float

takes_float(1.0)
takes_float(1)  # accepted: pylib.py uses Python's `float` special case
```

## `.by` exporter, `.py` consumer

A `.by` annotation rewrites to `JustFloat`, so a `.py` consumer importing it gets the strict type.
Passing an `int` is rejected.

`bylib.by`:

```by
def takes_float(x: float) -> None: ...
```

`consumer.py`:

```py
from bylib import takes_float

takes_float(1.0)
takes_float(1)  # error: [invalid-argument-type]
```

## composes with union

```by
def f(x: float | None) -> None: ...

f(1.0)
f(None)
f(1)  # error: [invalid-argument-type]
```

## shadowed `float` is left alone

A local rebinding shadows the builtin — the annotation keeps that local meaning.

```by
float = int
x: float = 1  # accepted: `float` here is the local alias for `int`
```

## arithmetic on a `float` stays a `float`

The promotion says an `int` is acceptable *where a `float` is asked for* — it is a rule about what a
position accepts. A return annotation accepts nothing, and `float.__mul__` returns a `float` and
never an `int`, so a promoted return would only invent a union. In a `.by` file, where the writer's
own `float` is strict, that union was then not assignable back to it, and basic numeric code did not
type-check at all.

```by
import math

def scaled(x: float) -> float:
    return x * 2.0

def magnitude(x: float, y: float) -> float:
    return math.sqrt(x * x + y * y)

def size(x: float) -> float:
    return abs(x)

def flip(x: float) -> float:
    return x if x > 0 else -x

def f(x: float, y: float) -> None:
    reveal_type(x * y)  # revealed: float
    reveal_type(x + y)  # revealed: float
    reveal_type(x / y)  # revealed: float
    reveal_type(-x)  # revealed: float
    reveal_type(math.sqrt(x))  # revealed: float
    reveal_type(round(x, 2))  # revealed: float
```

A stub *parameter* still promotes, so mixing an `int` into the arithmetic is accepted exactly as
python accepts it.

```by
def f(x: float) -> float:
    return x * 9 / 5 + 32
```

## a `.py` file still promotes on both sides

Tightening only a hand-written `.py` file's returns would break the pairing its parameters rely on:
`x` below reads as `int | float`, and a strict return could not accept it.

```py
def flip(x: float) -> float:
    return -x

def f(x: float) -> None:
    reveal_type(x)  # revealed: int | float
    reveal_type(-x)  # revealed: int | float
```
