# unused return value

a call written as a statement of its own keeps whatever the call did on the way to its answer and
throws the answer away. `unused-return-value` reports that, unless the declaration behind the call
says its result is optional.

the mdtest corpus turns this rule off — a test that checks how a call binds writes the call as a
statement of its own — so the tests below turn it back on.

```toml
[environment]
python-version = "3.12"

[rules]
unused-return-value = "warn"
```

## a discarded result is reported

```py
def parse(text: str) -> int:
    return 1

parse("1")  # error: [unused-return-value]
```

## a result that is used is not

anything the value flows into counts: a binding, an argument, an operand, a return.

```py
def parse(text: str) -> int:
    return 1

def use() -> int:
    n = parse("1")
    print(parse("2"))
    if parse("3"):
        pass
    return parse("4") + n
```

## a function returning `None` has no result to discard

```py
def log(message: str) -> None:
    return None

log("started")
```

## a call that never returns

`Never` is what a call that does not come back answers with, so there is no result anything could
have used.

```py
from typing import Never

def fail() -> Never:
    raise ValueError

fail()
```

## a gradual result is not known to be one

an unannotated function, or one declared to return `Any`, has not said it produces anything.

```py
from typing import Any

def untyped(): ...
def gradual() -> Any: ...

untyped()
gradual()
```

## a gradual result with a `None` in it is still gradual

a call reached through an untyped receiver answers with the gradual type unioned against whatever
the checker does know, which for a method declared to return `None` is `Any | None`. nothing is
known to have been discarded there either, so it is no more reported than a bare `Any` — while a
union whose arms are all known still is.

```py
from typing import Any

def gradual_or_none() -> Any | None: ...
def known_or_none() -> int | None: ...

gradual_or_none()
known_or_none()  # error: [unused-return-value]
```

## a method is reported like any other call

```py
class Parser:
    def parse(self, text: str) -> int:
        return 1

p = Parser()
p.parse("1")  # error: [unused-return-value]
Parser.parse(p, "1")  # error: [unused-return-value]
```

## constructing a class is a call

```py
class Widget:
    def __init__(self, size: int) -> None:
        self.size = size

Widget(1)  # error: [unused-return-value]
```

## a callable with no declaration behind it

a `Callable` parameter carries no declaration that could mark its result optional, so it keeps the
default.

```py
from typing import Callable

def run(action: Callable[[], int]) -> None:
    action()  # error: [unused-return-value]
```

## `@ignorable_return_value` on a function

the marker is available without importing it, and lowering leaves no trace of it in the emitted
python.

```by
@ignorable_return_value
def prime_cache(key: str) -> bytes:
    return b""

prime_cache("k")
```

## `@ignorable_return_value` on a class covers what the class body defines

each step of a fluent builder answers with the receiver it was given, and constructing one answers
with the builder.

```by
@ignorable_return_value
class Query:
    def where(self, clause: str) -> Query:
        return self

Query()
Query().where("id = 1")
```

## `@must_use_return_value` puts one member back

a builder's terminal operation is the result the whole chain was for.

```by
@ignorable_return_value
class Query:
    def where(self, clause: str) -> Query:
        return self

    @must_use_return_value
    def rows(self) -> list[str]:
        return []

q = Query()
q.where("id = 1")
q.rows()  # error: [unused-return-value]
```

## a declaration carrying both markers must use its result

`must_use_return_value` is the more specific of the two, so it decides.

```by
@ignorable_return_value
@must_use_return_value
def parse(text: str) -> int:
    return 1

parse("1")  # error: [unused-return-value]
```

## a subclass inherits what it did not override

```by
@ignorable_return_value
class Base:
    def step(self) -> int:
        return 1

class Derived(Base):
    def restep(self) -> int:
        return 1

d = Derived()
d.step()
d.restep()  # error: [unused-return-value]
```

## an override answers as the member it overrides did

a caller holding the base was allowed to drop what the base declared, and an override cannot take
that back. this is what makes `os.environ.setdefault(...)` — an `os._Environ` override of
`MutableMapping.setdefault` — read as the mapping method a caller wrote against.

```by
@ignorable_return_value
class Base:
    def step(self) -> int:
        return 1

    def settle(self) -> int:
        return 2

class Derived(Base):
    override def step(self) -> int:
        return 3

    @must_use_return_value
    override def settle(self) -> int:
        return 4

Derived().step()
Derived().settle()  # error: [unused-return-value]
```

## the stdlib override a `manage.py` is made of

```py
import os

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "project.settings")
```

## a marked class calling its own members

deciding whether a method's result may be dropped reads the class the method is defined in, and the
statement doing the dropping can be inside that very class.

```by
@ignorable_return_value
class Chain:
    def step(self) -> int:
        return 1

    def run(self) -> None:
        self.step()
        Chain().step()
```

## the marker travels with the declaration across modules

`a.by`:

```by
@ignorable_return_value
def prime_cache(key: str) -> bytes:
    return b""

@ignorable_return_value
class Query:
    def where(self, clause: str) -> Query:
        return self

def parse(text: str) -> int:
    return 1
```

`b.by`:

```by
from a import Query, parse, prime_cache

prime_cache("k")
Query().where("id = 1")
parse("1")  # error: [unused-return-value]
```

## a coroutine is `unused-awaitable`'s to report

what went missing is the `await`, not the use of what it would have produced. once awaited, the
result is an ordinary one.

```py
async def fetch() -> bytes:
    return b""

async def main() -> None:
    fetch()  # error: [unused-awaitable]
    await fetch()  # error: [unused-return-value]
```

## a value that no call produced

a bare expression statement discards a value too, but nothing was called to produce it, so there is
no declaration this could be about.

```py
def f(a: int) -> None:
    a
    a + 1
    "a docstring-shaped string"
```

## a marker is a decorator and nothing else

both markers are available without importing them, so a file that writes one where a *type* goes has
written an ordinary name that nothing defines.

```by
a: ignorable_return_value  # error: [unresolved-reference]
```

## the standard library carries the markers

the members whose result is idiomatically thrown away are marked in basedpython's own stubs.

```py
import subprocess

entries = [1, 2]
entries.pop()
{"a": 1}.setdefault("b", 2)
subprocess.run(["ls"])

"done".strip()  # error: [unused-return-value]
```
