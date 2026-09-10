# empty declarations

class and function declarations may be written without a body. basedpython
fills in `: ...` at transpile time so the output is valid python:

```by
class Empty
class Stub(Base)

protocol Reader:
    def read(self) -> str
```

transpiles to:

```python
class Empty: ...
class Stub(Base): ...

class Reader(Protocol):
    def read(self) -> str: ...
```

## scope

the bodyless form is recognized for both `class` and `def`. an empty class
gets `: ...`. a single empty `def` not in an overload run also gets `: ...`.
empty defs that *are* part of an overload run instead receive
`@overload` decorators (see [overloads](overloads.md))

an `abstract def` with no body is given `: raise NotImplementedError` rather
than `: ...`

a `def` with no body is a declaration, so it is allowed where a declaration is
what the position asks for: a stub file, a `Protocol` member, an abstract
method, an overload, or an `if TYPE_CHECKING` block. anywhere else the
implementation the signature promises was never written — the `: ...` it lowers
to just returns `None` — and that is reported (`missing-function-body`):

```by
def parse(s: str) -> int      # ok — the run below makes this an overload
def parse(s: bytes) -> int
def parse(s):
    return int(s)

def lookup() -> int           # error: no body
```

the return type makes no difference: `def lookup()` is reported the same way.
a function that is meant to do nothing says so with a body of its own,
`def ignore(event: str): ...`

an empty `class` is reported nowhere: a class with no members is a whole class,
with nothing left out

## interaction with modifiers

modifiers stack as expected:

```by
final class Sentinel
data class Empty
```

```python
@final
class Sentinel: ...

@dataclass(slots=True)
class Empty: ...
```

## why

a great deal of stub and protocol code consists of declaration-only
signatures. allowing `class Foo` / `def foo()` without a placeholder body
avoids `: ...` noise, particularly in stub-heavy modules and overload
clusters
