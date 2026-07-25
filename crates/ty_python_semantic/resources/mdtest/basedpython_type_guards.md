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
    if x is None:
        raise ValueError

def f(a: int | None):
    reveal_type(a)  # revealed: int | None
    check(a)
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## an assertion guard's value is only `None`

It raises when the assertion doesn't hold, which is why using its value gets no narrowing.

```by
def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError

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
    if second is None:
        raise ValueError

def f(a: object, b: int | None):
    check(a, b)
    reveal_type(b)  # revealed: int & not AlwaysFalsy
    reveal_type(a)  # revealed: object
```

## an asserted parameter is found through a keyword argument

```by
def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError

def f(a: int | None):
    check(x=a)
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## a method asserts its own parameter

```by
class C:
    def check(self, y: int | None) -> asserts y:
        if y is None:
            raise ValueError

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
    if a is None:
        raise ValueError

def m():
    check()
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## an awaited assertion narrows too

```by
async def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError

async def f(a: int | None):
    await check(a)
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## an assertion narrows an attribute

```by
class Holder:
    value: int | None = None

def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError

def f(h: Holder):
    check(h.value)
    reveal_type(h.value)  # revealed: int & not AlwaysFalsy
```

## an assertion only reaches the code after the call

```by
def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError

def f(a: int | None, flag: bool):
    if flag:
        check(a)
        reveal_type(a)  # revealed: int & not AlwaysFalsy
    reveal_type(a)  # revealed: int | None
```

## a later assignment ends an assertion

```by
def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError

def f(a: int | None, b: int | None):
    check(a)
    a = b
    reveal_type(a)  # revealed: int | None
```

## an unpacked argument doesn't say which parameter it reaches

```by
def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError

def f(args: list[int | None]):
    check(*args)
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

## `and` asserts every place it names

```by
def check(a: int | None, b: str | None) -> asserts a is int and b:
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
        if a is None:
            raise ValueError

    check()
    reveal_type(a)  # revealed: int & not AlwaysFalsy
```

## an assertion guard is called as a statement

Testing its value gets no narrowing — the value is `None`, so the test is always false.

```by
def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError

def f(a: int | None):
    if check(a):  # error: [narrowing-guard-as-value]
        reveal_type(a)  # revealed: Never
```

## binding an assertion guard's value gets no narrowing either

```by
def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError

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
    if x is None:
        raise ValueError
```
