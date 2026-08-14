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

The vendored typeshed is `.byi`, so nothing is promoted while reading it: the stubs spell out what
they accept instead. `float.__mul__` takes an `int | float` because `1.0 * 2` works, and returns a
`float` because the result never is an `int`. So arithmetic composes with a strict `float` the way
the writer expects.

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

A stub parameter says `int | float`, so mixing an `int` into the arithmetic is accepted exactly as
python accepts it.

```by
def f(x: float) -> float:
    return x * 9 / 5 + 32
```

## a value read out of the standard library is exact

A stub position a value only ever comes *out* of — a property, a module constant, a named-tuple
field — is a `float` and not a union. There is nothing to accept there, so there is nothing for the
typing spec's `int` to be doing.

```by
import math
import sys
import time

reveal_type(.0.real)  # revealed: float
reveal_type(.0.imag)  # revealed: float
reveal_type(math.pi)  # revealed: float
reveal_type(math.inf)  # revealed: float
reveal_type(sys.float_info.epsilon)  # revealed: float
reveal_type(time.time())  # revealed: float
```

## a standard-library parameter accepts an `int`

`math.sqrt(2)` really does work, so the stub says `int | float` rather than leaving the `int` to the
typing spec. The same goes for every other parameter upstream typeshed wrote as a bare `float`.

```by
import math

reveal_type(math.sqrt(2))  # revealed: float
reveal_type(math.hypot(3, 4.0))  # revealed: float
reveal_type(round(1.0, 2))  # revealed: float
```

## a constrained type variable is solved from the argument

`statistics.mean` is generic over a constraint list that upstream writes as `float`, meaning
`int | float`. The constraint is reached by the argument a call supplies, so it is widened like a
parameter and a list of `int`s still has a mean.

```by
import statistics

reveal_type(statistics.mean([1, 2, 3]))  # revealed: int | float
reveal_type(statistics.mean([1.0, 2.0]))  # revealed: int | float
```

## an attribute says what the library really keeps in it

An attribute is read as well as written, so the stub cannot answer the question by position alone —
it has to say what the library actually stores. `socketserver` documents `timeout` as a knob to set
and only ever forwards it to a selector, so an `int` is one of the things it holds.

```by
import socketserver

class Server(socketserver.TCPServer):
    timeout = 5

def f(server: socketserver.TCPServer) -> None:
    reveal_type(server.timeout)  # revealed: int | float | None
    server.timeout = 5
```

An attribute the library computes, rather than one it is handed, keeps the type it computes.

```by
import os

def f(st: os.stat_result) -> None:
    reveal_type(st.st_atime)  # revealed: float
    reveal_type(st.st_size)  # revealed: int
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

## `strict-float` gives a `.py` module the same model

The special case is what stops a `.py` module being compiled to unboxed doubles: `x: float` declares
`int | float`, so a field or a list element has to be able to hold either. `strict-float` opts a
module out of it, per module, through the ordinary `[[overrides]]` matching.

Unlike mypyc — which compiles `float` to a native double and silently converts an `int` argument —
this is a *checking* change first. You ask for the strict model and the checker holds you to it.

```toml
[analysis]
strict-float = true
```

```py
def scale(x: float) -> float:
    reveal_type(x)  # revealed: float
    return x * 2.0

scale(1.5)
scale(1)  # error: [invalid-argument-type]

a: float = 1.0
b: float = 1  # error: [invalid-assignment]
```

## an *inferred* attribute type is strict too

The special case is applied in two places: to an annotation, and again when an inferred type is
widened. An attribute assigned in `__init__` goes through the second one, so without this it would
come back `int | float` however strict the parameter that fed it was — and a field that might hold
either cannot be laid out as a `double`.

```toml
[analysis]
strict-float = true
```

```py
class Vec:
    def __init__(self, x: float, y: float) -> None:
        self.x = x
        self.y = y

    def norm2(self) -> float:
        reveal_type(self.x)  # revealed: float
        return self.x * self.x + self.y * self.y
```

## a literal still widens — only `float` and `complex` are held exact

```toml
[analysis]
strict-float = true
```

```py
class Counter:
    def __init__(self) -> None:
        self.n = 0
        self.label = "start"

    def read(self) -> None:
        reveal_type(self.n)  # revealed: int
        reveal_type(self.label)  # revealed: str
```

## a container's element type, which is what the compiler lays out

Reading an element out of a `list[float]` and iterating one both go through the same annotation, so
both follow the setting. This is the pair the native compiler depends on: an element it can only
describe as `int | float` cannot live in an unboxed buffer.

```toml
[analysis]
strict-float = true
```

```py
def total(xs: list[float]) -> float:
    reveal_type(xs[0])  # revealed: float
    out = 0.0
    for v in xs:
        reveal_type(v)  # revealed: float
        out = out + v
    return out
```

## and without it, the same reads give the union

```py
def total(xs: list[float]) -> float:
    reveal_type(xs[0])  # revealed: int | float
    for v in xs:
        reveal_type(v)  # revealed: int | float
    return 0.0
```

## `complex` too, from the same setting

```toml
[analysis]
strict-float = true
```

```py
def phase(z: complex) -> complex:
    reveal_type(z)  # revealed: complex
    return z

phase(1j)
phase(1.0)  # error: [invalid-argument-type]
```

## without it, a `.py` module keeps the special case

```py
def scale(x: float) -> float:
    reveal_type(x)  # revealed: int | float
    return x * 2.0

scale(1)

a: float = 1
```

## an inferred container element is strict in `.by`, with no setting to ask for it

`.by` has the strict numeric model by definition, so the widening an *inferred* type goes through
has to take the strict path there whatever the configuration says. this is the pair the native
compiler lays a buffer out from: an element it can only describe as `int | float` is not a `double`.

```by
def appended(n: int) -> None:
    xs = []
    xs.append(n * 0.5)
    reveal_type(xs)  # revealed: list[float]

def looped(n: int) -> None:
    xs = []
    i = 0
    while i < n:
        xs.append(i * 0.5)
        i = i + 1
    reveal_type(xs)  # revealed: list[float]

def handed(m: float) -> None:
    xs = []
    xs.append(m)
    reveal_type(xs)  # revealed: list[float]
```

## the same source as `.py` gives the union, unless it asks

```py
def appended(n: int) -> None:
    xs = []
    xs.append(n * 0.5)
    reveal_type(xs)  # revealed: list[int | float]
```

## and with `strict-float` it does not

```toml
[analysis]
strict-float = true
```

```py
def appended(n: int) -> None:
    xs = []
    xs.append(n * 0.5)
    reveal_type(xs)  # revealed: list[float]
```

## a display's element follows the model too, not only an appended one

The element type a *display* settles on goes through its own promotion, separate from the one an
`append` drives. Both have to follow the file, or `[m]` and `[]` + `append(m)` disagree about the
same list.

```by
def displayed(m: float) -> None:
    reveal_type([m])  # revealed: list[float]
    reveal_type([m * 2.0])  # revealed: list[float]
    reveal_type([1.0])  # revealed: list[float]
    reveal_type({m: m})  # revealed: dict[float, float]
    reveal_type({m})  # revealed: set[float]

def bound(n: int) -> None:
    xs = [n * 0.5]
    reveal_type(xs)  # revealed: list[float]
```

## `strict-float` gives a `.py` module the same displays

```toml
[analysis]
strict-float = true
```

```py
def displayed(m: float) -> None:
    reveal_type([m])  # revealed: list[float]
    reveal_type([1.0])  # revealed: list[float]

def bound(n: int) -> None:
    xs = [n * 0.5]
    reveal_type(xs)  # revealed: list[float]
```

## and without it the display widens, like everything else

```py
def displayed(m: float) -> None:
    reveal_type([m])  # revealed: list[int | float]

def bound(n: int) -> None:
    xs = [n * 0.5]
    reveal_type(xs)  # revealed: list[int | float]
```

## a generic call does not widen the argument it was handed

Binding a call promotes the solutions carried over from the previous fixpoint round, so that a
literal widens rather than accumulating. That promotion reads the *argument's* file: a caller whose
numeric model is strict must not get `int | float` back as the context its own argument is then read
against. Without this a `list[float]` handed to any `def f[T](xs: list[T])` came back
`list[float | int]`, and the native compiler lost the buffer.

```by
def consume[T](xs: list[T]) -> int:
    return len(xs)

def pair[T](xs: list[T], ys: list[T]) -> int:
    return len(xs) + len(ys)

def caller(n: int) -> int:
    xs = [0.0]
    xs.append(n * 0.5)
    reveal_type(xs)  # revealed: list[float]
    total = consume(xs)
    reveal_type(xs)  # revealed: list[float]
    ys = [1.0]
    total = total + pair(xs, ys)
    reveal_type(ys)  # revealed: list[float]
    return total
```

## `strict-float` gives a `.py` module the same

```toml
[analysis]
strict-float = true

[environment]
python-version = "3.12"
```

```py
def consume[T](xs: list[T]) -> int:
    return len(xs)

def caller(n: int) -> int:
    xs = [0.0]
    xs.append(n * 0.5)
    total = consume(xs)
    reveal_type(xs)  # revealed: list[float]
    return total
```

## and without it the union is the right answer, so it stays

```toml
[environment]
python-version = "3.12"
```

```py
def consume[T](xs: list[T]) -> int:
    return len(xs)

def caller(n: int) -> int:
    xs = [0.0]
    xs.append(n * 0.5)
    total = consume(xs)
    reveal_type(xs)  # revealed: list[int | float]
    return total
```
