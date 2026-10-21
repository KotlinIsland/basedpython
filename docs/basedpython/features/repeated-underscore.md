# repeated `_` parameters

basedpython lets `_` appear more than once in a function or lambda signature, so
a signature that ignores several arguments doesn't need invented placeholders.
the `_`s are passed by position:

```by
def on_event(_: int, _: str):
    print("fired")

on_event(1, "a")
```

transpiles to:

```python
def on_event(_: int, _2: str, /):
    print("fired")

on_event(1, "a")
```

python rejects two parameters of one name, so each `_` after the first is given
a name of its own — `_2`, `_3` and so on, skipping any name the module spells.
that name is the transpiler's rather than yours, so no call may spell it: a `/`
after the last `_` makes them positional-only, and passing one by keyword is a
`positional-only-parameter-as-kwarg` error

python only exempts `_`: duplicates of any other name remain a syntax error

## parameters after the last `_`

the `/` goes after the last `_`, so a parameter declared after it is still
passed by name:

```by
def ignore(_: int, _: int, label: str): ...

ignore(1, 2, label="a")
```

a `*_` or `**_` is reached by no keyword, so a `_` repeated only by one of them
is left as it is

## parameters before the last `_`

python's `/` makes every parameter before it positional-only, so a parameter you
named ahead of a repeated `_` would stop being reachable by its name. that is
refused rather than done quietly, as `invalid-repeated-underscore`:

```by
def pair(a: int, _: int, b: int, _: int): ...  # error
```

writing the `/` yourself says it is what you meant:

```by
def pair(a: int, _: int, b: int, _: int, /): ...
```

a method's receiver is the exception: nothing passes `self` by keyword, so
`def handle(self, _: int, _: int)` is accepted, with `self` before the `/`

a repeated `_` after `*` would be keyword-only, which only a keyword reaches, and
it has no name of its own to be reached by. that is refused as well

python before 3.8 has no `/`, so a project that targets it cannot make the `_`s
positional-only, and a signature that needs the `/` is refused there

## callable types and protocol methods

a callable type that names its parameters, and a method of an inline protocol,
follow the same rules, so a callback that ignores several arguments is called by
position:

```by
type Handler = (_: int, _: str) -> None
```

transpiles to:

```python
from typing import Protocol
class _Callable_f9c80db4(Protocol):
    def __call__(self, _: "int", _2: "str", /) -> "None": ...

type Handler = _Callable_f9c80db4
```

## overrides

a method that overrides another takes the names the overridden method gives
those positions, so a call written against the base keeps working on the
override:

```by
class Handler:
    def handle(self, event: int, source: str): ...

class Quiet(Handler):
    override def handle(self, _: int, _: str): ...

Quiet().handle(event=1, source="a")
```

transpiles to:

```python
from typing_extensions import override

class Handler:
    def handle(self, event: int, source: str): ...

class Quiet(Handler):
    @override
    def handle(self, event: int, source: str): ...

Quiet().handle(event=1, source="a")
```

a base parameter that is positional-only stays so, and a default the base gives
a parameter comes with it, as it does for any override (see
[inherited default values](inherited-defaults.md))

a base that repeats `_` itself gives the override its first `_`. the names it
numbers the others with are positional-only, so no call passes one, and the
override numbers its own `_`s there, clear of every name its module spells —
the names its body reads among them. a base that is itself such an override
gives the names it took

where the base has no parameter at some `_`'s position, none of the `_`s take a
name from it and they are numbered as above. that override is reported as an
`invalid-method-override` anyway

a name the base's author wrote would shadow the same name read from an enclosing
scope, so an override whose body reads one is refused as
`invalid-repeated-underscore`

## reading `_`

the `_`s are there to be ignored. python keeps only one parameter under the name
`_`, so a read of `_` could mean any of them, and reading one is an
`invalid-repeated-underscore` error:

```by
def pick(_: int, _: str) -> str:
    return _  # error
```

name the parameter you want to read instead. this covers every read the
parameters may reach: in the body, in `_ += 1`, and in a nested function, lambda
or comprehension that reads `_` from the function around it. an override is no
exception, though its `_`s take the base's names in the python

a read of `_` that the body has bound again on every path to it reads that
binding, and is fine:

```by
def count(_: int, _: int) -> int:
    _ = 0
    return _
```

so is `del _`, which reads no value, and a single `_` parameter, which is read
like any other parameter

## writing `_`

each `_` becomes a parameter of its own name in the python, so none of their
annotations is the type of the `_` the body binds. a write to `_` is checked
against none of them: it is an ordinary local variable, and a read that only the
write reaches has the type of the value written:

```by
def first(_: int, _: str) -> int:
    _ = 5  # not checked against `int` or `str`
    return _
```

the same goes for every other way of binding `_`: unpacking, `for _ in`, `:=`,
`with ... as _`, and a nested function's `nonlocal _`. `_ += 1` is no write of
this kind, since it reads `_` first, and is an error like any other read

an annotation the body writes for `_` declares it, as it would any local, so a
write after `_: bytes = b""` has to be a `bytes`
