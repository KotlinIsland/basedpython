# implicit object repr

a class that defines nothing the site can use falls back to the interpreter's own default —
`<module.Class object at 0x102bcc6a0>` for an instance, `<function f at 0x...>` for a function.
printing that is almost never what was meant, so it is reported wherever the rendering reaches the
output.

which dunder counts depends on what the site asks for: the fallbacks run one way only, so
`object.__str__` calls `__repr__` and `object.__format__` calls `str`, but nothing falls back to
`__str__`.

```toml
[environment]
python-version = "3.12"
```

## a class with no rendering of its own

```py
class Point:
    def __init__(self, x: int):
        self.x = x

print(Point(1))  # error: [implicit-object-repr]
str(Point(1))  # error: [implicit-object-repr]
repr(Point(1))  # error: [implicit-object-repr]
format(Point(1))  # error: [implicit-object-repr]
f"at {Point(1)}"  # error: [implicit-object-repr]
```

## `__repr__` answers every site

the fallbacks all end there: `object.__str__` calls `__repr__`, and `object.__format__` calls `str`.

```py
class Labelled:
    def __repr__(self) -> str:
        return ""

print(Labelled())
str(Labelled())
repr(Labelled())
ascii(Labelled())
format(Labelled())
f"{Labelled()}{Labelled()!s}{Labelled()!r}{Labelled()!a}"
```

## `__str__` answers everything but `repr`

nothing falls back to `__str__`, so `repr` still reaches the default.

```py
class Spoken:
    def __str__(self) -> str:
        return ""

print(Spoken())  # ok
str(Spoken())  # ok
format(Spoken())  # ok
f"{Spoken()}{Spoken()!s}"  # ok

repr(Spoken())  # error: [implicit-object-repr]
ascii(Spoken())  # error: [implicit-object-repr]
f"{Spoken()!r}"  # error: [implicit-object-repr]
f"{Spoken()!a}"  # error: [implicit-object-repr]
```

## `__format__` answers only the sites that go through it

```py
class Formatted:
    def __format__(self, spec: str, /) -> str:
        return ""

f"{Formatted()}"  # ok
format(Formatted())  # ok

print(Formatted())  # error: [implicit-object-repr]
str(Formatted())  # error: [implicit-object-repr]
repr(Formatted())  # error: [implicit-object-repr]
```

## `=` renders through `__repr__`

`f"{x=}"` writes the expression and then `repr(x)`. a spec of its own switches it back to `format`,
and an explicit conversion wins outright.

```py
class Spoken:
    def __str__(self) -> str:
        return ""

    def __format__(self, spec: str, /) -> str:
        return ""

f"{Spoken()=}"  # error: [implicit-object-repr]

f"{Spoken()=:>10}"  # ok — a spec makes it `format`
f"{Spoken()=!s}"  # ok — the conversion wins
```

## a stub is never reported

a stub describes an interface and never runs, so no rendering of it reaches anyone.

```pyi
class Point: ...

x = f"{Point()}"
```

## an inherited rendering counts

```py
class Base:
    def __repr__(self) -> str:
        return ""

class Derived(Base):
    pass

print(Derived())
```

## every positional argument of `print` is checked

```py
class Point:
    pass

class Named:
    def __repr__(self) -> str:
        return ""

# error: [implicit-object-repr]
# error: [implicit-object-repr]
print(Point(), Named(), Point())
```

## `str(buffer, encoding)` decodes rather than renders

the two-argument `str` is a different function that happens to share a name, and no `__str__` runs.

```py
print(str(memoryview(b"hello"), "utf-8"))  # ok
print(str(b"hello", encoding="utf-8"))  # ok

print(str(memoryview(b"hello")))  # error: [implicit-object-repr]
```

## `print`'s keyword arguments are not rendered

```py
class Sink:
    def write(self, s: str, /) -> int:
        return 0

    def flush(self) -> None: ...

print("x", file=Sink())
```

## a builtin is never reported

a stub leaves these dunders out whether or not the runtime class has them, so nothing can be
concluded from their absence.

```py
print(1, 1.5, "s", b"b", [1], {1: 2}, (1,), {1})
```

## a function and a class object are reported by default

`types.FunctionType` and `builtins.type` are the two stubs taken at their word out of the box — they
declare no rendering, and neither has one written for it.

```py
def f(): ...

class A: ...

print(f)  # error: [implicit-object-repr]
print(A)  # error: [implicit-object-repr]
print(int)  # error: [implicit-object-repr]
```

## the stdlib classes that print an address are reported

membership is decided by asking a real interpreter whether `repr(v)` contains `hex(id(v))`, not by
whether the stub declares a `__repr__` — the two come apart in both directions.

```py
import contextlib
import itertools
import threading

print(f"{(y for y in [])}")  # error: [implicit-object-repr]
print(map(str, []))  # error: [implicit-object-repr]
print(zip([1], [2]))  # error: [implicit-object-repr]
print(enumerate([1]))  # error: [implicit-object-repr]
print(itertools.chain([1]))  # error: [implicit-object-repr]
print(threading.Event())  # error: [implicit-object-repr]
print(contextlib.ExitStack())  # error: [implicit-object-repr]
```

## a stdlib class that prints well is left alone

`threading.Thread` and `itertools.count` declare no `__repr__` either, and print perfectly.

```py
import io
import itertools
import threading

print(threading.Thread())
print(itertools.count())
print(open("x"))
print({}.keys())
print(io)
```

## a listed class settles its whole hierarchy

the entry is a claim about how instances actually print, which already accounts for what the class
inherits — so a stub base in the way does not make it undecidable again. `GeneratorType` derives
from the `Generator` abc, and neither supplies a rendering.

```py
async def work() -> None: ...

print((y for y in []))  # error: [implicit-object-repr]
print(work())  # error: [implicit-object-repr]
```

## an abstract type is not a concrete class

a value declared as `Generator` or `Iterator` could be any implementation, including one that writes
a `__repr__`, so nothing can be concluded from the declaration alone.

```py
from collections.abc import Generator, Iterator

def f(a: Generator[int, None, None], b: Iterator[int]):
    print(a, b)
```

## a subclass that writes a rendering is quiet

```py
import threading

class Announced(threading.Event):
    def __repr__(self) -> str:
        return "Announced()"

print(Announced())
```

## a class named in `report-types` is taken at its word

```toml
[environment]
python-version = "3.12"

[analysis]
implicit-object-repr-report-types = ["types.ModuleType"]
```

```py
import types

print(types)  # error: [implicit-object-repr]
```

## a class named in `exempt-types` is never reported

exempt wins over the report list and over the source-class rule alike.

```toml
[environment]
python-version = "3.12"

[analysis]
implicit-object-repr-exempt-types = ["types.FunctionType", "opaque.Handle"]
```

`opaque.py`:

```py
class Handle:
    pass
```

```py
from opaque import Handle

def f(): ...

print(f)
print(Handle())
```

## an exempt base exempts what derives from it

```toml
[environment]
python-version = "3.12"

[analysis]
implicit-object-repr-exempt-types = ["opaque.Handle"]
```

`opaque.py`:

```py
class Handle:
    pass
```

```py
from opaque import Handle

class MyHandle(Handle):
    pass

print(MyHandle())
```

## a bare builtin name matches

```toml
[environment]
python-version = "3.12"

[analysis]
implicit-object-repr-exempt-types = ["type"]
```

```py
class A: ...

print(A)
```

## a stub beside an implementation is still only read as a stub

when a module has both, ty type-checks against the stub, and the stub is what this asks. that is the
right answer when the implementation does define a rendering the stub omits — which is the usual
reason it is omitted.

`shown.pyi`:

```pyi
class Shown: ...
```

`shown.py`:

```py
class Shown:
    def __repr__(self) -> str:
        return "Shown()"
```

```py
from shown import Shown

print(Shown())  # ok — the implementation has a `__repr__` the stub did not mention
```

## a class whose implementation also lacks one is a known miss

nothing in either file defines a rendering, so this prints an address at runtime. it is not
reported, because only the stub is consulted — reading the implementation would settle it, at the
cost of a dependency on a file that type checking does not otherwise open.

```toml
[environment]
python-version = "3.12"
```

`bare.pyi`:

```pyi
class Bare: ...
```

`bare.py`:

```py
class Bare:
    pass
```

```py
from bare import Bare

# TODO: prints `<bare.Bare object at 0x...>`, and the implementation says so
print(Bare())
```

## a class inheriting from a stub class is not reported

```py
class MyList(list[int]):
    pass

print(MyList())
```

## a dataclass gets a `__repr__` without writing one

```py
from dataclasses import dataclass

@dataclass
class Point:
    x: int

print(Point(1))
```

## a dataclass that turned its `__repr__` off is reported

```py
from dataclasses import dataclass

@dataclass(repr=False)
class Point:
    x: int

print(Point(1))  # error: [implicit-object-repr]
```

## a subclass of a dataclass inherits the synthesized `__repr__`

```py
from dataclasses import dataclass

@dataclass
class Base:
    x: int

class Derived(Base):
    pass

print(Derived(1))
```

## `object()` itself is not reported

asking for `object()`'s repr is asking for exactly what it gives.

```py
print(object())
```

## a conversion does not excuse the value

```py
class Point:
    pass

# the conversion is what produces the bare repr in the first place
str(Point())  # error: [implicit-object-repr]
f"{Point()!r}"  # error: [implicit-object-repr]
```

## an exactly-constructed value is reported

`A()` is inferred as `final A`, and it is `A` that has or lacks a rendering.

```by
class A:
    pass

def main():
    a = A()
    print(a)  # error: [implicit-object-repr]
    print(A())  # error: [implicit-object-repr]
```

## a value of unknown type is not reported

```py
from typing import Any

def f(a: Any, b):
    print(a, b)
```
