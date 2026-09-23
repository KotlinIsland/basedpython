# Repeated `_` parameters

basedpython relaxes the python rule that all parameter names must be unique: when the duplicated
name is `_`, the parameters are allowed. Python itself refuses a repeated parameter, so the
transpiler gives every `_` after the first a name of its own (`_2`, `_3`, ...) that the module
spells nowhere. Those names are the transpiler's rather than the author's, so a call cannot spell
them: the numbered parameters are positional-only, and the signature ty checks a call against is the
one the transpiled python has. The signature shows each of them as the `_` the author wrote.
References to `_` inside the body resolve to the first parameter.

A module that spells `_2` anywhere has its `_`s numbered past it, so the examples below that pass a
numbered name by keyword declare the function in a module of its own.

## allowed in basedpython

```by
def f(_, _):
    reveal_type(_)  # revealed: _@f

def g(_, _, _, x: int):
    reveal_type(x)  # revealed: int
```

## still rejected for non-`_` names

```by
# error: [invalid-syntax] "Duplicate parameter "x""
def f(x: int, x: str) -> int:
    return 1
```

## still rejected in python

```py
# error: [invalid-syntax] "Duplicate parameter "_""
def f(_, _):
    return 1
```

## numbered parameters are positional-only

A `/` follows the last numbered `_`, so every parameter up to it is passed by position:

`pairs.by`:

```by
def pair(_: int, _: str) -> None: ...
```

`main.by`:

```by
from pairs import pair

reveal_type(pair)  # revealed: def pair(_: int, _: str, /)

pair(1, "a")
# error: [positional-only-parameter-as-kwarg]
pair(1, _2="a")
# error: [positional-only-parameter-as-kwarg]
# error: [positional-only-parameter-as-kwarg]
pair(_=1, _2="a")
```

## a diagnostic names a repeated `_` by its position

Several parameters are called `_`, and every one after the first has a name in the python the author
never wrote, so a diagnostic about one says which it is by position:

```by
def pair(_: int, _: str) -> None: ...

# error: [missing-argument] "No argument provided for required parameter 2 (`_`) of function `pair`"
pair(1)
# error: [missing-argument] "No arguments provided for required parameters 1 (`_`), 2 (`_`)"
pair()
```

## a named parameter after the last `_` keeps its keyword

The `/` goes after the last `_`, so a parameter declared after it is still reachable by name:

`ignoring.by`:

```by
def ignore(_: int, _: int, label: str) -> None: ...
```

`main.by`:

```by
from ignoring import ignore

reveal_type(ignore)  # revealed: def ignore(_: int, _: int, /, label: str)

ignore(1, 2, label="a")
```

## a variadic `_` needs no `/`

No keyword reaches a `*_` or a `**_`, so a `_` repeated only by one of them stays as written:

`rests.by`:

```by
def rest(_: int, *_: int) -> None: ...
```

`main.by`:

```by
from rests import rest

reveal_type(rest)  # revealed: def rest(_: int, *_: int)

rest(_=1)
```

## a method's receiver

A method's receiver is never passed by keyword, so the `/` making it positional-only changes nothing
a caller writes:

`handlers.by`:

```by
class Handler:
    def handle(self, _: int, _: int) -> None: ...
```

`main.by`:

```by
from handlers import Handler

reveal_type(Handler.handle)  # revealed: def handle(self, _: int, _: int, /)

Handler().handle(1, 2)
```

## a lambda

`lambdas.by`:

```by
h = lambda _, _: 1
```

`main.by`:

```by
from lambdas import h

reveal_type(h)  # revealed: (_, _, /) -> 1
```

## a callable type

A callable type that names its parameters is declared in python as a protocol's `__call__`, whose
parameter list follows the same rule, so a call reaches the numbered `_` by position alone:

`callbacks.by`:

```by
type Callback = (_: int, _: str, name: bytes) -> None
```

`main.by`:

```by
from callbacks import Callback

def call(f: Callback) -> None:
    reveal_type(f)  # revealed: (_: int, _: str, /, name: bytes) -> None
    f(1, "a", name=b"")
    # error: [positional-only-parameter-as-kwarg]
    f(1, _2="a", name=b"")
```

## an inline protocol's method

A method of an inline protocol is declared as a method of the protocol class it lowers to, and its
receiver is exempt as a `def`'s is:

`handlers.by`:

```by
type Handler = protocol(def handle(self, _: int, _: str) -> None)
```

`main.by`:

```by
from handlers import Handler

def call(h: Handler) -> None:
    reveal_type(h.handle)  # revealed: (_: int, _: str, /) -> None
    h.handle(1, "a")
    # error: [positional-only-parameter-as-kwarg]
    h.handle(1, _2="a")
```

## a keyword-only repeated `_` in a callable type is refused

A parameter after `*args` is reached only by keyword, as it is in a `def`:

```by
# error: [invalid-repeated-underscore] "a repeated `_` parameter cannot be keyword-only"
type Keyword = (int, /, _: int, *args: int, _: str) -> None
```

## a named parameter before the last `_` is refused

Python's `/` makes every parameter before it positional-only. A parameter the author named would
stop being reachable by that name, which is theirs to decide, so the definition is refused:

```by
# error: [invalid-repeated-underscore] "the repeated `_` parameters after `a` make it positional-only"
def pair(a: int, _: int, b: int, _: int) -> None: ...
```

A lambda is refused by the same rule:

```by
# error: [invalid-repeated-underscore] "the repeated `_` parameters after `a` make it positional-only"
first = lambda a, _, _: a
```

## writing the `/` is accepted

`explicit.by`:

```by
def pair(a: int, _: int, b: int, _: int, /) -> None: ...
```

`main.by`:

```by
from explicit import pair

reveal_type(pair)  # revealed: def pair(a: int, _: int, b: int, _: int, /)
```

## a `/` written before the last `_`

A `/` the source wrote before the last `_` ends up after it:

`moved.by`:

```by
def pair(a: int, /, _: int, _: int) -> None: ...
```

`main.by`:

```by
from moved import pair

reveal_type(pair)  # revealed: def pair(a: int, _: int, _: int, /)
```

## a keyword-only repeated `_` is refused

A parameter after `*` is reached only by keyword, and a repeated `_` has no name of its own to be
reached by:

```by
# error: [invalid-repeated-underscore] "a repeated `_` parameter cannot be keyword-only"
def keyword(_: int, *, _: int) -> None: ...
```

## a target without the `/` is refused

Python 3.8 added the `/`, and before it nothing makes a parameter positional-only. A numbered `_`
there would be reached by the number the lowering gave it, so the definition is refused instead:

```toml
[environment]
python-version = "3.7"
```

```by
# error: [invalid-repeated-underscore] "a repeated `_` parameter needs a `/`, which python before 3.8 does not have"
def pair(_: int, _: str) -> None: ...
```

## an override takes the base method's names

A method that overrides one takes the names the overridden method gives those positions, so a call
written against the base keeps working on the override:

```by
class Base:
    def f(self, x: int, y: str) -> None: ...

class Override(Base):
    override def f(self, _: int, _: str) -> None: ...

reveal_type(Override.f)  # revealed: def f(self, x: int, y: str)

Override().f(x=1, y="a")
base: Base = Override()
base.f(x=1, y="a")
```

## a positional-only base parameter stays so

The kind comes from the base along with the name:

```by
class Base:
    def f(self, x: int, /, y: str) -> None: ...

class Override(Base):
    override def f(self, _: int, _: str) -> None: ...

reveal_type(Override.f)  # revealed: def f(self, x: int, /, y: str)

Override().f(1, y="a")
# error: [positional-only-parameter-as-kwarg]
Override().f(x=1, y="a")
```

## an override with defaults

Parameters named after the base take its defaults too, as any override that re-declares one does:

```by
class Base:
    def f(self, x: int = 1, y: str = "a") -> None: ...

class Override(Base):
    override def f(self, _: int, _: str) -> None: ...

reveal_type(Override.f)  # revealed: def f(self, x: int = 1, y: str = "a")

Override().f()
```

## an override of a method whose `_`s are numbered

A base that repeats `_` numbers its own, and an override takes those names and kinds as it takes any
other. The first is `_` in both, so a read of `_` in the override's body is its first parameter, as
it is in any definition that repeats `_`:

```by
class Base:
    def f(self, _: int, _: int) -> int:
        return _

class Override(Base):
    override def f(self, _: int, _: int) -> int:
        return _ * 10

reveal_type(Override.f)  # revealed: def f(self, _: int, _: int, /) -> int

assert Override().f(1, 2) == 10
```

A variadic `_` has no counterpart in the base, and is numbered after the names the others take:

```by
class Variadic(Base):
    override def f(self, _: int, _: int, *_: int) -> int:
        return 20

reveal_type(Variadic.f)  # revealed: def f(self, _: int, _: int, /, *_: int) -> int

assert Variadic().f(1, 2, 3) == 20
```

## an override of an override

An override that repeats `_` has the names of the method it overrides, so an override of it takes
those same names in turn:

```by
class Base:
    def f(self, x: int, y: int) -> int:
        return 1

class Middle(Base):
    override def f(self, _: int, _: int) -> int:
        return 2

class Override(Middle):
    override def f(self, _: int, _: int) -> int:
        return _ * 30

reveal_type(Override.f)  # revealed: def f(self, x: int, y: int) -> int

assert Override().f(x=1, y=2) == 30
```

## a base parameter named `_`

A base parameter that its author named `_` is reached by that keyword, so the override's `_` in that
position keeps the name rather than being numbered:

```by
class Base:
    def f(self, x: int, _: int) -> int:
        return x

class Override(Base):
    override def f(self, _: int, _: int) -> int:
        return _

reveal_type(Override.f)  # revealed: def f(self, x: int, _: int) -> int

assert Override().f(1, _=2) == 1
```

## an override of a base in another module

`base.by`:

```by
class Base:
    def f(self, x: int, y: int) -> int:
        return 1
```

`main.by`:

```by
from base import Base

class Override(Base):
    override def f(self, _: int, _: int) -> int:
        return _ * 40

reveal_type(Override.f)  # revealed: def f(self, x: int, y: int) -> int

assert Override().f(x=1, y=2) == 40
```

## an override of a generic base

The names come from the base whether or not the override specializes it:

```by
class Base[T]:
    def f(self, x: T, y: T) -> int:
        return 1

class Specialized(Base[int]):
    override def f(self, _: int, _: int) -> int:
        return _ * 50

class Generic[T](Base[T]):
    override def f(self, _: T, _: T) -> int:
        return 60

reveal_type(Specialized.f)  # revealed: def f(self, x: int, y: int) -> int
reveal_type(Generic[str]().f)  # revealed: bound method Generic[str].f(x: str, y: str) -> int

assert Specialized().f(x=1, y=2) == 50
assert Generic[str]().f(x="a", y="b") == 60
```

## an override that does not line up with its base is numbered

Where the base has no parameter at a `_`'s position, none of the `_`s take a name from it. The
override is then an invalid one, and is reported as such:

`overrides.by`:

```by
class Base:
    def f(self, x: int) -> None: ...

class Override(Base):
    # error: [invalid-method-override]
    override def f(self, _: int, _: int) -> None: ...
```

`main.by`:

```by
from overrides import Override

reveal_type(Override.f)  # revealed: def f(self, _: int, _: int, /)
```

## an inherited name the body reads is refused

A parameter named after the base would shadow a name the body reads from an enclosing scope.
Refused, the override keeps no names a caller of the base could pass by keyword, which is reported
too:

```by
x = 1

class Base:
    def f(self, x: int, y: int) -> int:
        return x

class Override(Base):
    # error: [invalid-repeated-underscore] "a repeated `_` parameter named `x` shadows a `x` the body reads"
    # error: [invalid-method-override]
    override def f(self, _: int, _: int) -> int:
        return x
```
