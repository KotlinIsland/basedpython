# string formatting

an f-string replacement field is a call — `f"{value:spec}"` runs
`type(value).__format__(value, "spec")` — so the spec is checked like any other argument, and
its content is checked against the `__format__` that reads it

```py
class Point: ...

f"{Point()}"  # ok — the empty spec is the one every type accepts
f"{Point():>10}"  # error: `Point` has no format spec to apply
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

[format specification mini-language]: https://docs.python.org/3/library/string.html#format-specification-mini-language
