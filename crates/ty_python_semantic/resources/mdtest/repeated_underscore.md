# Repeated `_` parameters

basedpython relaxes the python rule that all parameter names must be unique: when the duplicated
name is `_`, the parameters are allowed. Python itself refuses a repeated parameter, so the
transpiler gives every `_` after the first a name of its own (`_2`, `_3`, ...) that the module
spells nowhere. Those names are the transpiler's rather than the author's, so a call cannot spell
them: the numbered parameters are positional-only, and the signature ty checks a call against is the
one the transpiled python has. The signature shows each of them as the `_` the author wrote. Reading
`_` where it may be one of them is an error, since python and ty would each read a different one.
Writing `_` is checked against none of their annotations, since none of them is the `_` the body
binds.

A module that spells `_2` anywhere has its `_`s numbered past it, so the examples below that pass a
numbered name by keyword declare the function in a module of its own.

## allowed in basedpython

```by
def f(_, _):
    return 1

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

## reading a repeated `_` is an error

Python binds `_` to only one of the parameters, the lowering keeping the first `_` under that name,
while every other `_` shadows the one before it as ty reads the body. A read of `_` would mean the
first parameter to python and the last to ty, so it is an error rather than either:

```by
def f(_: int, _: str) -> str:
    # error: [invalid-repeated-underscore] "reading a repeated `_` parameter of `f`"
    return _
```

Having no one meaning, the read has no one type either, so it is typed `Unknown` rather than as the
last `_`:

```by
def g(_: int, _: str) -> None:
    # error: [invalid-repeated-underscore]
    reveal_type(_)  # revealed: Unknown
```

## a single `_` is read as usual

A parameter list that names one parameter `_` binds it like any other parameter, and a module's own
`_` is a name like any other too:

```by
def f(_: int) -> int:
    return _

_ = 2

assert f(1) == 1
assert _ == 2
```

## a write to `_` is checked against no parameter

The lowering gives every `_` a parameter name of its own, so none of their annotations declares the
`_` the body binds. A write to `_` is an assignment to an undeclared local, and a read that only it
reaches sees the value written:

```by
def f(_: int, _: str) -> int:
    _ = 5
    reveal_type(_)  # revealed: 5
    return _

assert f(1, "a") == 5
```

A starred `_` declares nothing either, though its annotation gives the parameter a tuple type:

```by
def rest(_: int, *_: str) -> int:
    _ = 5
    return _

assert rest(1, "a", "b") == 5
```

## every binding of `_` is such a write

Unpacking, a `for` loop, an assignment expression and a `with` statement all bind `_` without a
declared type to check it against:

```by
import tempfile

def unpack(_: int, _: str) -> int:
    a, _ = 1, 2
    return a + _

def loop(_: int, _: str) -> int:
    total = 0
    for _ in range(3):
        total += _
    return total

def walrus(_: int, _: str) -> int:
    if (_ := 3) > 0:
        return _
    return 0

def with_(_: int, _: str) -> bool:
    with tempfile.TemporaryFile() as _:
        return _.closed

assert unpack(1, "a") == 3
assert loop(1, "a") == 3
assert walrus(1, "a") == 3
assert not with_(1, "a")
```

A lambda's `_`s declare nothing, whether or not the lambda is typed:

```by
untyped = lambda _, _: (_ := 9)
typed = lambda (_: int, _: str) -> int: (_ := 9)

assert untyped(1, "a") == 9
assert typed(1, "a") == 9
```

So a nested function that declares `nonlocal _` writes it unchecked too:

```by
def outer(_: int, _: str) -> int:
    def inner() -> int:
        nonlocal _
        _ = 7
        return _

    return inner()

assert outer(1, "a") == 7
```

## an annotated `_` declares it

An annotation the body writes for `_` is the one declaration of the name, as it is for any local, so
a write after it is checked against it:

```by
def f(_: int, _: str) -> bytes:
    _: bytes = b"a"
    # error: [invalid-assignment] "Object of type `5` is not assignable to `bytes`"
    _ = 5
    return _
```

## a read that only a rebinding of `_` reaches

The body may bind `_` itself. A read that only that binding reaches reads it, whatever the
parameters are:

```by
def f(_: int, _: int) -> int:
    _ = 5
    return _

assert f(1, 2) == 5
```

## a read that one path from the parameters still reaches

A rebinding on some paths leaves the parameters reaching the read on the others, so the read is an
error:

```by
def f(_: int, _: int, flag: bool) -> int:
    if flag:
        _ = 5
    # error: [invalid-repeated-underscore]
    return _
```

## an augmented assignment reads `_`

`_ += 1` reads `_` before it binds it:

```by
def f(_: int, _: int) -> None:
    # error: [invalid-repeated-underscore]
    _ += 1
```

The refused read is typed `Unknown`, so the operator is not checked against the last `_` as well: a
`str` has no `+` with an `int`, but the read is the one error here:

```by
def g(_: int, _: str) -> None:
    # error: [invalid-repeated-underscore]
    _ += 1
```

## `del _` reads no value

`del _` unbinds the name rather than reading its value, so whichever parameter python and ty take it
to be, both are left with `_` unbound:

```by
def f(_: int, _: str) -> None:
    del _

f(1, "a")
```

## a nested scope that reads it

A nested function, a lambda and a comprehension read a name they do not bind from the function
around them, so each read of `_` there is an error too:

```by
def f(_: int, _: str) -> None:
    def g() -> str:
        # error: [invalid-repeated-underscore] "reading a repeated `_` parameter of `f`"
        return _

    # error: [invalid-repeated-underscore]
    h = lambda: _

    # error: [invalid-repeated-underscore]
    xs = [_ for x in range(3)]
```

A nested function reads the name when it is called, by which point the function around it may have
bound `_` again, so its read is an error wherever a binding of the parameters remains:

```by
def later(_: int, _: int) -> None:
    def g() -> int:
        # error: [invalid-repeated-underscore]
        return _
    _ = 5
```

## a nested scope that binds `_` itself

A nested scope that binds `_` reads its own:

```by
def f(_: int, _: str) -> list[int]:
    def g(_: bytes) -> bytes:
        return _

    assert g(b"a") == b"a"
    return [_ for _ in range(3)]

assert f(1, "a") == [0, 1, 2]
```

## `nonlocal _` and `global _`

A nested function that declares `nonlocal _` reads the parameters, unless it binds `_` on every path
to the read first:

```by
def f(_: int, _: int) -> None:
    def reads() -> int:
        nonlocal _
        # error: [invalid-repeated-underscore]
        return _

    def rebinds() -> int:
        nonlocal _
        _ = 5
        return _
```

One that declares `global _` reads the module's `_`:

```by
_ = 5

def g(_: int, _: str) -> int:
    def module() -> int:
        global _
        return _

    return module()

assert g(1, "a") == 5
```

## a read the parser writes

`init(let _: int, let _: int)` stands for a `self._ = _` for each parameter, which the author never
wrote. The lowering writes each of them reading its own parameter, by the name it gives that
parameter, so neither is a read of a repeated `_`:

```by
class Pair:
    init(let _: int, let _: int)

assert Pair(1, 2)._ == 2
```

## a lambda's own repeated `_`

A lambda's parameters are read by the same rule:

```by
# error: [invalid-repeated-underscore] "reading a repeated `_` parameter of a lambda"
first = lambda _, _: _
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

A base that repeats `_` numbers its own. The first is `_` in the base, and the override's `_` in
that position takes that name as it takes any other. The numbered ones are positional-only, so no
call passes one by name, and the override numbers its own there:

```by
class Base:
    def f(self, _: int, _: int) -> int:
        return 1

class Override(Base):
    override def f(self, _: int, _: int) -> int:
        return 10

reveal_type(Override.f)  # revealed: def f(self, _: int, _: int, /) -> int

assert Override().f(1, 2) == 10
```

## a variadic `_` in an override of a method whose `_`s are numbered

A variadic `_` has no counterpart in the base, and is numbered after the names the others take:

```by
class Base:
    def f(self, _: int, _: int) -> int:
        return 1

class Variadic(Base):
    override def f(self, _: int, _: int, *_: int) -> int:
        return 20

reveal_type(Variadic.f)  # revealed: def f(self, _: int, _: int, /, *_: int) -> int

assert Variadic().f(1, 2, 3) == 20
```

## a name the base numbered is the override's to number

The name the base's lowering gives a `_` is no name a caller passes, so the override does not take
it. Its own `_` there is numbered clear of the names its module spells, which include every name its
body reads:

`base.by`:

```by
class Base:
    def f(self, _: int, _: int) -> int:
        return 1
```

`main.by`:

```by
from base import Base

_2 = 5

class Override(Base):
    override def f(self, _: int, _: int) -> int:
        return _2  # error: [used-underscore-name]

reveal_type(Override.f)  # revealed: def f(self, _: int, _: int, /) -> int

assert Override().f(1, 2) == 5
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
        return 30

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
        return 35

reveal_type(Override.f)  # revealed: def f(self, x: int, _: int) -> int

assert Override().f(1, _=2) == 35
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
        return 40

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
        return 50

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

## an override's body

An override's `_`s take the names of the base's parameters, but the body still spells them all `_`,
so a read of `_` there is an error as it is anywhere else:

```by
class Base:
    def f(self, x: int, y: str) -> str:
        return y

class Override(Base):
    override def f(self, _: int, _: str) -> str:
        # error: [invalid-repeated-underscore]
        return _
```

## `del _` in an override's body

`del _` reads no value in an override's body either, though its `_`s take the names of the base's
parameters:

```by
class Base:
    def f(self, x: int, y: str) -> str:
        return y

class Deletes(Base):
    override def f(self, _: int, _: str) -> str:
        del _
        return "deleted"

assert Deletes().f(1, "a") == "deleted"
```
