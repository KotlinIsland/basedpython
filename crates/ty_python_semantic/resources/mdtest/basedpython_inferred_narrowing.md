# basedpython: narrowing a function never wrote down

`def is_int(a: object) -> a is int` states what a truthy result means about the argument. A `def`
that leaves its return type out states the same thing by returning `a is int`, and its callers
narrow by it too — the claim is recovered from the body alongside the return type itself.

## a returned predicate narrows the argument it is about

```by
def is_int(a: object):
    return a is int

def f(x: object):
    reveal_type(is_int)  # revealed: def is_int(a: object) -> TypeIs[int]
    if is_int(x):
        reveal_type(x)  # revealed: int
    else:
        reveal_type(x)  # revealed: not int
```

## the predicate can be any test the checker already reads

`isinstance` narrows a place wherever it is written, so returning it says the same thing about the
argument.

```by
def is_int(a: object):
    return isinstance(a, int)

def f(x: object):
    if is_int(x):
        reveal_type(x)  # revealed: int
```

## a negated test narrows the other way

What a truthy result means and what a falsy one means are recovered separately, so a test that rules
a type out narrows on the side that rules it back in.

```by
def missing(a: int | None):
    return a is None

def f(x: int | None):
    if missing(x):
        reveal_type(x)  # revealed: None
    else:
        reveal_type(x)  # revealed: int
```

## the recovered guard follows the parameter, not its position

A written `TypeIs` narrows a function's first parameter. What the body tested is what a recovered
guard narrows, whichever parameter that is.

```by
def second_is_str(first: object, second: object):
    return second is str

def f(a: object, b: object):
    if second_is_str(a, b):
        reveal_type(b)  # revealed: str
        reveal_type(a)  # revealed: object
```

A keyword argument reaches the same parameter, so it narrows the same place.

```by
def g(a: object, b: object):
    if second_is_str(second=b, first=a):
        reveal_type(b)  # revealed: str
```

## a test on a member narrows that member

The place a guard names can reach below the parameter, and it is resolved against the member of
whatever the call passed.

```by
class Holder:
    data: str | None

def loaded(h: Holder):
    return h.data is str

def f(h: Holder):
    if loaded(h):
        reveal_type(h.data)  # revealed: str
```

## a method's test narrows the receiver it was called on

The first parameter of a bound call is the receiver rather than an argument, so a guard rooted at it
follows the value the call was made on.

```by
class Holder:
    data: str | None

    def loaded(self):
        return self.data is str

def f(h: Holder):
    if h.loaded():
        reveal_type(h.data)  # revealed: str
```

## several places are narrowed at once

Each place is recovered on its own, so a conjunction narrows every place it tests where the call is
truthy. A falsy result only says the conjunction failed, which says nothing about any one of them.

```by
def both(a: object, b: object):
    return a is int and b is str

def f(x: object, y: object):
    if both(x, y):
        reveal_type(x)  # revealed: int
        reveal_type(y)  # revealed: str
    else:
        reveal_type(x)  # revealed: object
        reveal_type(y)  # revealed: object
```

A disjunction is the mirror image: a falsy result rules both out, and a truthy one settles neither.

```by
def either(a: object, b: object):
    return a is int or b is str

def g(x: object, y: object):
    if either(x, y):
        reveal_type(x)  # revealed: object
    else:
        reveal_type(x)  # revealed: not int
        reveal_type(y)  # revealed: not str
```

## returns that agree are unioned

Each `return` that can hand back a truthy value contributes what it establishes, and the guard is
what they add up to.

```by
def numeric(a: object, flag: bool):
    if flag:
        return a is int
    return a is float

def f(x: object):
    if numeric(x, True):
        reveal_type(x)  # revealed: int | float
```

## every return has to agree

A body narrows a place where the call is truthy only if every `return` that can hand back a truthy
value narrows it. Here the second one does not, so a truthy result says nothing.

```by
def maybe(a: object, flag: bool):
    if flag:
        return a is int
    return True

def f(x: object):
    if maybe(x, True):
        reveal_type(x)  # revealed: object
```

## falling off the end says nothing

Control reaching the end of the body hands back `None`, which is falsy, so a falsy result no longer
means the test failed. The truthy side is untouched.

```by
def is_int(a: object, flag: bool):
    if flag:
        return a is int

def f(x: object):
    if is_int(x, True):
        reveal_type(x)  # revealed: int
    else:
        reveal_type(x)  # revealed: object
```

## only a predicate is recovered from

A result that is a value in its own right is not a claim about the arguments, whatever its
truthiness.

```by
def identity(a: object):
    return a

def f(x: object):
    if identity(x):
        reveal_type(x)  # revealed: object
```

## a coroutine and a generator are not what their body returns

`is_int(x)` on an `async def` evaluates to a coroutine object, which is truthy without the body
having run at all, so testing it says nothing about the argument.

```by
async def is_int(a: object):
    return a is int

def yields_int(a: object):
    yield a is int

def f(x: object, y: object):
    reveal_type(is_int)  # revealed: def is_int(a: object) -> CoroutineType[Any, Any, bool]
    if is_int(x):
        reveal_type(x)  # revealed: object
    if yields_int(y):
        reveal_type(y)  # revealed: object
```

## an override is not handed a narrowing its own body does not make

A base's return type stands for its overrides, but a narrowing return type is a claim about what the
body tests. An override that tests something else would be handed a claim it does not make, so that
one is left to the override's own body — and the override reports the difference where it is, rather
than narrowing wrongly at every call.

```by
class Base:
    def check(self, a: object):
        return a is int

class Child(Base):
    # error: [invalid-method-override]
    def check(self, a: object):
        return bool(a)

def f(c: Child, x: str):
    reveal_type(Child.check)  # revealed: def check(self, a: object) -> bool
    if c.check(x):
        reveal_type(x)  # revealed: str & not AlwaysFalsy
```

## an overload group is the whole answer

The overloads say what the function returns, so its implementation's body is not read for a
narrowing the overloads did not declare.

```by
from typing import overload

@overload
def is_int(a: object) -> bool: ...
@overload
def is_int(a: object, b: int) -> bool: ...
def is_int(a, b=0):
    return a is int

def f(x: object):
    if is_int(x):
        reveal_type(x)  # revealed: object
```

## a written return type is the whole answer

Nothing is recovered beside an annotation, so a `def` that says it returns `bool` returns exactly
that.

```by
def is_int(a: object) -> bool:
    return a is int

def f(x: object):
    if is_int(x):
        reveal_type(x)  # revealed: object
```

## a recovered guard travels through a variable

A single place narrowed both ways is what `TypeIs` means, so that is how the recovered return type
is written — and a result held in a variable carries it.

```by
def is_int(a: object):
    return a is int

def f(x: object):
    result = is_int(x)
    reveal_type(result)  # revealed: TypeIs[int @ x]
    if result:
        reveal_type(x)  # revealed: int
```

## an unannotated parameter is recovered alongside the guard

The parameter's own type is recovered from what the body does with it, and the guard is recovered
beside it.

```by
def is_str(x):
    return isinstance(x, str)

def f(a: object):
    if is_str(a):
        reveal_type(a)  # revealed: str
```

## a predicate that delegates to another one

The delegate's own guard is what its call narrows, so returning that call passes the guard along.

```by
def is_int(a: object):
    return a is int

def also_int(a: object):
    return is_int(a)

def f(x: object):
    if also_int(x):
        reveal_type(x)  # revealed: int
```

## a predicate that calls itself recovers nothing

What the recursive call means about the argument is the very thing being worked out, so while it is
being worked out it means nothing — and a `return` that says nothing about a place leaves it
unnarrowed. Answering otherwise would let a guard prove itself.

```by
def nested(a: object, depth: int):
    if depth == 0:
        return a is int
    return nested(a, depth - 1)

def f(x: object):
    if nested(x, 2):
        reveal_type(x)  # revealed: object
```

## a recovered guard narrows wherever a condition is tested

```by
def is_int(a: object):
    return a is int

def f(x: object, y: object, z: object):
    while is_int(x):
        reveal_type(x)  # revealed: int
    assert is_int(y)
    reveal_type(y)  # revealed: int
    if not is_int(z):
        reveal_type(z)  # revealed: not int
```

## a call whose arguments are unpacked narrows nothing

An unpacked argument list does not say which value reached the parameter.

```by
def is_int(a: object):
    return a is int

def f(args: tuple[object]):
    if is_int(*args):
        reveal_type(args)  # revealed: (object,)
```

## a recovered guard is checked like any other narrowing

An argument that cannot be what the guard tests for narrows to `Never`.

```by
def is_int(a: object):
    return a is int

def f(x: str):
    if is_int(x):
        reveal_type(x)  # revealed: Never
```

## a parameter the body puts something else in is not a guard

A guard names the argument a call passed, so it says nothing once the body puts something else where
that argument was. `rebound` hands back `True` whatever it is given, and reading that as a claim
about the argument would narrow it to `Never`.

```by
def rebound(a: object):
    a = 1
    return a is int

def f(x: str):
    reveal_type(rebound)  # revealed: def rebound(a: object) -> bool
    if rebound(x):
        reveal_type(x)  # revealed: str
```

Anything that binds the name counts, wherever in the body it is — an assignment on one branch, a
loop target, a nested scope writing through `nonlocal`.

```by
def conditionally(a: object, flag: bool):
    if flag:
        a = 1
    return a is int

def looped(a: object, xs: list[object]):
    for a in xs:
        pass
    return a is int

def through_a_nested_scope(a: object):
    def inner():
        nonlocal a
        a = 1

    inner()
    return a is int

def f():
    reveal_type(conditionally)  # revealed: def conditionally(a: object, flag: bool) -> bool
    reveal_type(looped)  # revealed: def looped(a: object, xs: list[object]) -> bool
    reveal_type(through_a_nested_scope)  # revealed: def through_a_nested_scope(a: object) -> bool
```

## a member the body writes to is not a guard either

What the body tested and what the caller reads back afterwards are two different values.

```by
class A:
    b: object

def written(a: A):
    result = a.b is int
    a.b = "s"
    return result

def f(x: A):
    reveal_type(written)  # revealed: def written(a: A) -> bool
    if written(x):
        reveal_type(x.b)  # revealed: object
```

## the members of a returned place travel with it

`assert a.b is int` narrows the place `a.b`, and returning `a` would ordinarily leave that behind.
The recovered return type says it structurally instead, so the caller reads the member back
narrowed.

```by
class A:
    b: object

def checked(a: A):
    assert a.b is int
    return a

def f(x: A):
    reveal_type(checked)  # revealed: def checked(a: A) -> A & protocol(b: int)
    reveal_type(checked(x).b)  # revealed: int
```

## a member below a member nests

A claim about `o.inner.b` is a claim about `o.inner` that is itself a claim about `b`.

```by
class Inner:
    b: object

class Outer:
    inner: Inner

def checked(o: Outer):
    assert o.inner.b is str
    return o

def f(x: Outer):
    reveal_type(checked)  # revealed: def checked(o: Outer) -> Outer & protocol(inner: protocol(b: str))
    reveal_type(checked(x).inner.b)  # revealed: str
```

## a member the flow does not settle is not claimed

A narrowing that only holds on one branch does not hold where the branches meet, so nothing is
claimed about it.

```by
class A:
    b: object

def maybe(a: A, flag: bool):
    if flag:
        assert a.b is int
    return a

def f(x: A):
    reveal_type(maybe)  # revealed: def maybe(a: A, flag: bool) -> A
```

## writing to a member ends what was established about it

```by
class A:
    b: object

def rewritten(a: A):
    assert a.b is int
    a.b = "s"
    return a

def f(x: A):
    reveal_type(rewritten)  # revealed: def rewritten(a: A) -> A
```

## a returned member carries what is below it

The claim describes the place that is handed back, so a returned member is described by what lies
below *it*.

```by
class Inner:
    c: object

class Outer:
    b: Inner

def checked(o: Outer):
    assert o.b.c is int
    return o.b

def f(x: Outer):
    reveal_type(checked)  # revealed: def checked(o: Outer) -> Inner & protocol(c: int)
```

## a type is handed back by a `type def`, not a value

A `type def` evaluates to the type it names, so there is no value whose members a caller could read
and nothing for a claim about them to describe.

```by
class A:
    b: object

type def Pick[X]:
    if X.name == "int":
        return int
    return str

def f(a: Pick[int]):
    reveal_type(a)  # revealed: int
```

## a returned place carries its own narrowing too

The place itself is narrowed by the ordinary rules, and its members ride along with it.

```by
class A:
    b: object

def checked(a: A | None):
    assert a is not None
    assert a.b is int
    return a

def f(x: A | None):
    reveal_type(checked)  # revealed: def checked(a: A | None) -> A & protocol(b: int)
```

## the same holds in a plain python file

`sound-types` recovers a signature in a `.py` file too, and the narrowing it recovers is the same.

```toml
[analysis]
sound-types = true
```

```py
def is_int(a: object):
    return isinstance(a, int)

def f(x: object):
    if is_int(x):
        reveal_type(x)  # revealed: int
```
