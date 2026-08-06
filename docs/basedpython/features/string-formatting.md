# string formatting

an f-string replacement field is a call — `f"{value:spec}"` runs
`type(value).__format__(value, "spec")` — so it is checked like one, and a value with nothing
to say about how it looks is reported before it reaches the output

```py
class Point:
    def __init__(self, x: int):
        self.x = x

f"{Point(1)}"  # warning: prints `<__main__.Point object at 0x...>`
f"{Point(1):>10}"  # error: `Point` has no format spec to apply
f"{'name':d}"  # error: `d` is not a presentation type for `str`
```

## the empty spec is the only one `object` accepts

`object.__format__` has nothing to interpret a spec with — it hands the value to `str` and
ignores everything else — so at runtime any non-empty spec on a class that defines no
`__format__` of its own raises `TypeError`. it is typed as accepting only `""`, and a spec is
reported as `invalid-format-spec`

that also means a class can now say exactly which specs it takes, which widens `object`'s
signature rather than narrowing it:

```py
from typing import Literal

class Temperature:
    def __format__(self, spec: Literal["", "c", "f"], /) -> str: ...

f"{Temperature():c}"  # ok
f"{Temperature():k}"  # error: [invalid-format-spec]
```

## the mini-language applies to the type that reads it

`str`, `int`, `float` and `complex` are the four `__format__` implementations that read the
[format specification mini-language], and each accepts a different part of it. the spec is
checked against whichever one the value actually reaches:

```py
f"{'name':,}"  # error: `str` has no grouping
f"{1:.2}"  # error: an integer has no precision
f"{1:.2f}"  # ok — `f` formats the integer as a float
f"{1.0:x}"  # error: `float` has no bases
f"{1:_x}"  # ok — `_` also groups the power-of-two bases
```

a type with a `__format__` of its own is checked only as a call, and none of these rules touch
it.

## `datetime` reads strftime directives

`date`, `time` and `datetime` hand the spec to `strftime`, so they get their own language rather
than none. an unrecognised directive does not raise — the platform writes it through and the
output is quietly wrong — which is the only reason it is worth checking:

```py
from datetime import datetime

d = datetime(2001, 2, 3)

f"{d:%Y-%m-%d}"  # ok
f"{d:>10}"  # ok — literal text here, not an alignment
f"{d:%-d}"  # ok — not portable, but deliberate often enough to leave alone
f"{d:%Q}"  # error: no platform renders `%Q`
f"{d:%Y%}"  # error: a `%` with no directive after it
```

a spec assembled at runtime is not checked at all, because there is nothing to check:

```py
def f(width: int):
    return f"{1.5:{width}.2f}"
```

## a conversion formats the `str` it produces

`!s`, `!r` and `!a` run before the spec does, and each produces a `str`, so the spec is read
by `str.__format__` rather than by the value's own:

```py
class Point: ...

f"{Point()!r:>10}"  # ok — a `str` takes a width
f"{Point()!r:d}"  # error: a `str` has no `d`
```

## a value with no rendering of its own

a class that defines nothing the site can use renders through `object.__repr__`, which prints
the class name and the address the object happens to sit at. that is reported as
`implicit-object-repr` wherever the rendering reaches the output — an f-string field, or a call
to `str`, `repr`, `ascii`, `format` or `print`

```py
class Point: ...

print(Point())  # warning: [implicit-object-repr]
```

which dunder counts depends on what the site asks for, because the fallbacks run one way only:
`object.__str__` calls `__repr__`, and `object.__format__` calls `str`, but nothing falls back
to `__str__`

| site                                          | answered by                         |
| --------------------------------------------- | ----------------------------------- |
| `repr(x)`, `ascii(x)`, `f"{x!r}"`, `f"{x!a}"` | `__repr__`                          |
| `str(x)`, `print(x)`, `f"{x!s}"`              | `__str__`, `__repr__`               |
| `format(x)`, `f"{x}"`                         | `__format__`, `__str__`, `__repr__` |

so a `__str__` on its own is not enough for `repr`:

```py
class Spoken:
    def __str__(self) -> str:
        return "Spoken()"

print(Spoken())  # ok
repr(Spoken())  # warning: [implicit-object-repr]
```

a `__repr__` answers every site, including one that is generated rather than written:

```py
from dataclasses import dataclass

@dataclass
class Point:
    x: int

print(Point(1))  # ok — `@dataclass` writes a `__repr__`

@dataclass(repr=False)
class Bare:
    x: int

print(Bare(1))  # warning: [implicit-object-repr]
```

only a class written in source is judged. a stub leaves these dunders out whether or not the
runtime class has them — `int` declares none of the three and still prints as a number — so a
class that comes from a stub, or that inherits from one, is not reported:

```py
class MyList(list[int]): ...

print(MyList())  # ok — nothing can be concluded from `list`'s stub
```

a hard-coded set of stdlib classes is taken at its word, though — the ones whose `repr` is
nothing but the class name and an address:

```py
import threading

def helper() -> None: ...

print(helper)  # warning: prints `<function helper at 0x...>`
print(zip([1], [2]))  # warning: prints `<zip object at 0x...>`
print((y for y in []))  # warning: prints `<generator object at 0x...>`
print(threading.Event())  # warning: prints `<threading.Event object at 0x...>`
```

membership is decided by asking a real interpreter whether `repr(v)` contains `hex(id(v))`,
which is not the same question as whether the stub declares a `__repr__` — the two come apart in
both directions. `generator` declares one and it is still an address; `threading.Thread` and
`itertools.count` declare none and print perfectly, so they are left alone:

```py
import itertools
import threading

print(threading.Thread())  # ok — `<Thread(Thread-1, initial)>`
print(itertools.count())  # ok — `count(0)`
print(open("data.txt"))  # ok — names the file and the mode
```

only the stdlib is listed. an extension class from anywhere else cannot be judged from its stub
and is left alone rather than guessed at, as is a value whose declared type is abstract —
a `Generator` could be any implementation, including one that writes a `__repr__`

the list is `analysis.implicit-object-repr-report-types`, and
`analysis.implicit-object-repr-exempt-types` is its opposite — a class named there is never
reported, and neither is anything deriving from it:

```toml
[tool.ty.analysis]
# a third-party class you know prints an address
implicit-object-repr-report-types = ["mylib.Handle"]
# and one of the defaults you would rather not hear about
implicit-object-repr-exempt-types = ["builtins.property"]
```

entries are qualified class names; a class in `builtins` may also be spelled bare (`type`).
setting `report-types` replaces the default list rather than adding to it

[format specification mini-language]: https://docs.python.org/3/library/string.html#format-specification-mini-language
