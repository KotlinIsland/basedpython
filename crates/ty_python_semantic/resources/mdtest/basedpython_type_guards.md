# basedpython: narrowing return annotations

A function's return annotation can name the place a call narrows. `-> x is T` is PEP 742 narrowing,
applied where the call evaluates truthy; `-> asserts x` narrows once the call returns, for the rest
of the flow.

## the annotation names which parameter is narrowed

`TypeIs` narrows a function's first parameter. Naming the parameter picks any of them.

```by
def is_str(first: object, second: object) -> second is str:
    return isinstance(second, str)

def f(a: object, b: object):
    if is_str(a, b):
        reveal_type(b)  # revealed: str
        reveal_type(a)  # revealed: object
```

## the named parameter is found through a keyword argument

```by
def is_str(first: object, second: object) -> second is str:
    return isinstance(second, str)

def f(a: object, b: object):
    if is_str(second=b, first=a):
        reveal_type(b)  # revealed: str
```

## a name that is not a parameter is a place

`def f() -> a is int` narrows `a` itself, so a predicate needs no argument to narrow.

```by
def src() -> int | None:
    return 1

a = src()

def f() -> a is int:
    return a is not None

def m():
    if f():
        reveal_type(a)  # revealed: int
    else:
        reveal_type(a)  # revealed: None
    reveal_type(a)  # revealed: int | None
```

## the place is resolved where the call is written

A local of that name is what narrows.

```by
def src() -> int | None:
    return 1

a = src()

def f() -> a is int:
    return a is not None

def m():
    a = src()
    if f():
        reveal_type(a)  # revealed: int
```

## a guard from another file names a place in that module

A same-named place here is a different symbol, so it narrows nothing.

`guard.by`:

```by
def src() -> int | None:
    return 1

a = src()

def f() -> a is int:
    return a is not None
```

`main.by`:

```by
from guard import f, src

a = src()

def m():
    if f():
        reveal_type(a)  # revealed: int | None
```

## a predicate on a place has no parameter to check

`TypeIs` must narrow a parameter, and the narrowed type must be assignable to it. Neither applies to
a place, whose type is whatever the calling scope has.

```by
a: str = "s"

def f() -> a is int:
    return True
```

## `asserts` narrows once the call returns

```by
def check(x: int | None) -> asserts x:
    assert x

def f(a: int | None):
    reveal_type(a)  # revealed: int | None
    check(a)
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## an assertion guard's value is only `None`

It raises when the assertion doesn't hold, which is why using its value gets no narrowing.

```by
def check(x: int | None) -> asserts x:
    assert x

def f(a: int | None):
    # error: [narrowing-guard-as-value] "an assertion guard narrows when it is called as a statement, and its value is only the `None` it returns"
    reveal_type(check(a))  # revealed: None
```

## `asserts not` narrows the other way

```by
def check_empty(x: str | None) -> asserts not x:
    if x:
        raise ValueError

def f(a: str | None):
    check_empty(a)
    reveal_type(a)  # revealed: (str & not AlwaysTruthy) | None
```

## `asserts x is T` narrows to a type

```by
def check(x: int | str | None) -> asserts x is int:
    if x is not int:
        raise ValueError

def f(a: int | str | None):
    check(a)
    reveal_type(a)  # revealed: int
```

## `asserts x is None` narrows to `None`

```by
def check(x: int | None) -> asserts x is None:
    if x is not None:
        raise ValueError

def f(a: int | None):
    check(a)
    reveal_type(a)  # revealed: None
```

## `asserts x is not T` removes a type

```by
def check(x: int | None) -> asserts x is not None:
    if x is None:
        raise ValueError

def f(a: int | None):
    check(a)
    reveal_type(a)  # revealed: int
```

## an asserted type must fit the parameter it narrows

```by
def check(x: int) -> asserts x is str:  # error: [invalid-type-guard-definition] "Narrowed type `str` is not assignable to the declared parameter type `int`"
    raise ValueError
```

Removing a type constrains nothing, so `is not` is unrestricted.

```by
def check(x: int) -> asserts x is not str:
    return None
```

## the body has to establish what it asserts

Every call narrows its argument once it returns, so a body that can return without having
established the assertion narrows an argument to something it may not be.

```by
# error: [unestablished-assertion-guard] "`x` is `int | None` where the body ends, but this function asserts it is truthy"
def check(x: int | None) -> asserts x:
    pass
```

Ruling out `None` does not establish truthiness: `0` is still falsy.

```by
# error: [unestablished-assertion-guard] "`x` is `int` where the body ends, but this function asserts it is truthy"
def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError
```

## each `return` has to establish it too

A `return` is reported where it is.

```by
def check(x: int | None) -> asserts x:
    if x is None:
        # error: [unestablished-assertion-guard] "`x` is `None` where this returns, but this function asserts it is truthy"
        return
    if not x:
        raise ValueError
```

A `return` after the assertion holds is fine, and so is a body that never returns at all.

```by
def check(x: int | None) -> asserts x:
    if x:
        return
    raise ValueError

def never(x: int | None) -> asserts x:
    raise ValueError
```

## a failure the body catches establishes nothing

```by
# error: [unestablished-assertion-guard] "`x` is `int | None` where the body ends, but this function asserts it is truthy"
def check(x: int | None) -> asserts x:
    try:
        assert x
    except AssertionError:
        pass
```

## the declared type is part of what the body establishes

Ruling out `None` leaves only `1`, which is truthy, so the assertion holds — where the same body
under `int | None` would leave `0`.

```by
from typing import Literal

def check(x: Literal[1] | None) -> asserts x:
    if x is None:
        raise ValueError
```

## removing a type has to be established too

```by
# error: [unestablished-assertion-guard] "`x` is `object` where the body ends, but this function asserts it is not `str`"
def require(x: object) -> asserts x is not str:
    pass
```

## a body that rebinds the parameter asserts nothing about the argument

The caller's argument is whatever it was; only the parameter holds what the body assigned.

```by
# error: [unestablished-assertion-guard] "the body puts another value in `x`, so what it establishes is not about the argument this guard narrows"
def check(x: int | None, y: int | None) -> asserts x:
    x = y
    assert x
```

## a member the body assigns is established

Unlike a parameter, a member is read back by the caller after the call, so an assignment is what it
reads.

```by
class Holder:
    data: int | None = None

    def load(self) -> asserts self.data is not None:
        self.data = 1

    # error: [unestablished-assertion-guard] "`self.data` is `int | None` where the body ends, but this function asserts it is not `None`"
    def forget(self) -> asserts self.data is not None:
        pass
```

## an assertion about a place is checked too

```by
def src() -> int | None:
    return 1

a = src()

def check() -> asserts a is not None:
    assert a is not None

# error: [unestablished-assertion-guard] "`a` is `int | None` where the body ends, but this function asserts it is not `None`"
def forget() -> asserts a is not None:
    pass
```

## a `def` that only declares one has no body to establish it

A `...` body says the function is declared elsewhere — in a stub file, as an `@overload`, or as a
member of a `Protocol` — so there is nothing there to establish the assertion. A `pass` body is an
implementation, and an empty one establishes nothing.

```by
from typing import Protocol

class Reader(Protocol):
    def ensure(self, x: int | None) -> asserts x: ...
```

## a `return` a `finally` suite can follow is left alone

A `finally` suite runs before the `return` hands control back, and can swallow the exception that
was on its way out or change what the caller reads. What a caller sees is the state after that
suite, which is not the state at the `return`, so such a body is neither read as establishing the
assertion nor reported for failing to.

```by
def check(x: int | None) -> asserts x:
    try:
        assert x
    finally:
        return
```

## an unannotated parameter is not an established one

`Any` fits every assertion, which is the reason it says nothing about one.

```by
from typing import Any

# error: [unestablished-assertion-guard] "`x` is `Any` where the body ends, but this function asserts it is truthy"
def check(x: Any) -> asserts x:
    pass
```

## ruling out every value of a type establishes the assertion

`bool` is `True` and `False` and nothing else, so ruling both out rules out `bool` itself.

```by
def check(x: object) -> asserts x is not bool:
    if x is True or x is False:
        raise ValueError
```

## a body that binds the place asserts nothing about the caller's

A local of the place's name is a different place from the one a call narrows.

```by
def src() -> int | None:
    return 1

a = src()

# error: [unestablished-assertion-guard] "the body puts another value in `a`, so what it establishes is not about the place this guard narrows"
def check() -> asserts a is not None:
    a = 1
```

Writing to the place itself is another matter: `global` names the place the caller reads.

```by
def src() -> int | None:
    return 1

b = src()

def check() -> asserts b is not None:
    global b
    b = 1
```

## rebinding what a member is read off ends what was established about it

`a.c` is read off `a`, so replacing it leaves what the body established about `a.c.b` describing a
value the caller never sees.

```by
class Inner:
    b: int | None = None

class Outer:
    c: Inner = Inner()

# error: [unestablished-assertion-guard] "`a.c.b` is `int | None` where the body ends, but this function asserts it is not `None`"
def check(a: Outer) -> asserts a.c.b is not None:
    assert a.c.b is not None
    a.c = Inner()
```

## an override has to establish what the method it overrides asserts

A call through the base narrows on the strength of the base's assertion, and what it is called on
may be an instance of the subclass, so the override makes the same assertion — and has to establish
it.

```by
from typing import override

class Base:
    def ensure(self, x: int | None) -> asserts x:
        assert x

class Silent(Base):
    @override
    # error: [unestablished-assertion-guard] "`x` is `int | None` where the body ends, but `Base.ensure`, which this overrides, asserts it is truthy"
    def ensure(self, x: int | None):
        pass

class Loud(Base):
    @override
    def ensure(self, x: int | None):
        assert x

def f(base: Base, loud: Loud, a: int | None, b: int | None):
    base.ensure(a)
    reveal_type(a)  # revealed: int & not AlwaysFalsy
    # the override carries the assertion for its own callers too
    loud.ensure(b)
    reveal_type(b)  # revealed: int & not AlwaysFalsy
```

## an override is free to rename a positional-only parameter

The guard names a parameter of the base by name, and follows it by position into the override, which
is what lets a positional-only parameter be renamed.

```by
from typing import override

class Base:
    def ensure(self, x: int | None, /) -> asserts x:
        assert x

class Renamed(Base):
    @override
    def ensure(self, value: int | None, /):
        assert value

def f(r: Renamed, a: int | None):
    r.ensure(a)
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## `asserts` can name a place with a type too

```by
def src() -> int | str | None:
    return 1

a = src()

def check() -> asserts a is int:
    if a is not int:
        raise ValueError

def m():
    check()
    reveal_type(a)  # revealed: int
```

## the asserted parameter is named, like a predicate

```by
def check(first: object, second: int | None) -> asserts second:
    assert second

def f(a: object, b: int | None):
    check(a, b)
    reveal_type(b)  # revealed: int & not AlwaysFalsy
    reveal_type(a)  # revealed: object
```

## an asserted parameter is found through a keyword argument

```by
def check(x: int | None) -> asserts x:
    assert x

def f(a: int | None):
    check(x=a)
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## a keyword of a positional-only parameter's name reaches a different parameter

`check(x, a=y)` passes `x` for `a` and collects `a=y` into `**kw`, so `x` is the argument the
assertion is about.

```by
def check(a: int | None = 1, /, **kw: str) -> asserts a:
    assert a

def f(x: int | None, y: str):
    check(x, a=y)
    reveal_type(x)  # revealed: int & not AlwaysFalsy
    reveal_type(y)  # revealed: str
```

## a `classmethod` is called on the class, not on the receiver

A bound method takes the value it was called on as its first parameter, so a guard on that
parameter's member narrows the receiver's. A `classmethod` takes the class instead, so `cls.value`
is not the place `c.value` names.

```by
class C:
    value: int | None = None

    @classmethod
    def ensure(cls) -> asserts cls.value is not None:
        assert cls.value is not None

def f(c: C):
    c.ensure()
    reveal_type(c.value)  # revealed: int | None
```

## a method asserts its own parameter

```by
class C:
    def check(self, y: int | None) -> asserts y:
        assert y

def f(c: C, a: int | None):
    c.check(a)
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## `asserts` can name a place too

```by
def src() -> int | None:
    return 1

a = src()

def check() -> asserts a:
    assert a

def m():
    check()
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## an awaited assertion narrows too

```by
async def check(x: int | None) -> asserts x:
    assert x

async def f(a: int | None):
    await check(a)
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## an assertion narrows an attribute

```by
class Holder:
    value: int | None = None

def check(x: int | None) -> asserts x:
    assert x

def f(h: Holder):
    check(h.value)
    reveal_type(h.value)  # revealed: int & not AlwaysFalsy
```

## an assertion only reaches the code after the call

```by
def check(x: int | None) -> asserts x:
    assert x

def f(a: int | None, flag: bool):
    if flag:
        check(a)
        reveal_type(a)  # revealed: int & not AlwaysFalsy
    reveal_type(a)  # revealed: int | None
```

## a later assignment ends an assertion

```by
def check(x: int | None) -> asserts x:
    assert x

def f(a: int | None, b: int | None):
    check(a)
    a = b
    reveal_type(a)  # revealed: int | None
```

## an unpacked argument doesn't say which parameter it reaches

```by
def check(x: int | None) -> asserts x:
    assert x

def f(args: list[int | None]):
    check(*args)  # error: [refutable-unpacking]
```

## a guard narrows a member of what it names

```by
class Holder:
    data: str | None = None

    def ensure(self) -> asserts self.data is not None:
        if self.data is None:
            raise ValueError

def f(h: Holder):
    h.ensure()
    reveal_type(h.data)  # revealed: str
```

## a predicate narrows a member too

```by
class Holder:
    data: str | None = None

    def loaded(self) -> self.data is str:
        return self.data is not None

def f(h: Holder):
    if h.loaded():
        reveal_type(h.data)  # revealed: str
```

## a member guard follows the receiver it was called on

```by
class Holder:
    data: str | None = None

    def ensure(self) -> asserts self.data is not None:
        if self.data is None:
            raise ValueError

class Outer:
    holder: Holder = Holder()

def f(o: Outer, other: Holder):
    o.holder.ensure()
    reveal_type(o.holder.data)  # revealed: str
    reveal_type(other.data)  # revealed: str | None
```

## a guard on a parameter's member narrows the argument's

```by
class Holder:
    data: str | None = None

def ensure(h: Holder) -> asserts h.data is not None:
    if h.data is None:
        raise ValueError

def f(a: Holder):
    ensure(a)
    reveal_type(a.data)  # revealed: str
```

## a receiver that was constructed is narrowed too

```by
class Holder:
    data: str | None = None

    def ensure(self) -> asserts self.data is not None:
        if self.data is None:
            raise ValueError

def f():
    h = Holder()
    h.ensure()
    reveal_type(h.data)  # revealed: str
```

## a guard called at module level narrows there

```by
class Holder:
    data: str | None = "ready"

    def ensure(self) -> asserts self.data is not None:
        if self.data is None:
            raise ValueError

h = Holder()
h.ensure()
reveal_type(h.data)  # revealed: str
```

## `and` asserts every place it names

```by
def check(a: int | None, b: str | None) -> asserts a is int and b:
    # error: [overlapping-condition]
    if a is None or not b:
        raise ValueError

def f(x: int | None, y: str | None):
    check(x, y)
    reveal_type(x)  # revealed: int
    reveal_type(y)  # revealed: str & not AlwaysFalsy
```

## a guard has to name a place that exists

```by
def check(value: int | None) -> asserts values:  # error: [unresolved-narrowing-guard] "`values` is neither a parameter nor a place here, so this guard narrows nothing"
    if value is None:
        raise ValueError
```

## a place the guard can see is enough, wherever it is written

```by
def src() -> int | None:
    return 1

def outer():
    a = src()

    def check() -> asserts a:
        assert a

    check()
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## an assertion guard is called as a statement

Testing its value gets no narrowing — the value is `None`, so the test is always false.

```by
def check(x: int | None) -> asserts x:
    assert x

def f(a: int | None):
    if check(a):  # error: [narrowing-guard-as-value]
        reveal_type(a)  # revealed: Never
```

## binding an assertion guard's value gets no narrowing either

```by
def check(x: int | None) -> asserts x:
    assert x

def f(a: int | None):
    ok = check(a)  # error: [narrowing-guard-as-value]
    reveal_type(a)  # revealed: int | None
```

## a predicate is the guard whose value is the test

So it is unaffected.

```by
def is_str(x: object) -> x is str:
    return isinstance(x, str)

def f(a: object):
    ok = is_str(a)
    reveal_type(ok)  # revealed: TypeIs[str @ a]
```

## `asserts` must name a place

```by
def check(x: int) -> asserts 1 + 1:  # error: [invalid-type-form] "`asserts` must name a place, optionally negated with `not` or tested against a type with `is`"
    return None
```

## `asserts` is basedpython syntax

```py
def check(x: int | None) -> asserts x:  # error: [invalid-syntax] "`asserts` return annotations are not valid in .py files"
    assert x
```
