# basedpython: narrowing a function never wrote down

`def is_int(a: object) -> a is int` states what a truthy result means about the argument. A `def`
that leaves its return type out states the same thing by returning `a is int`, and its callers
narrow by it too — the claim is recovered from the body alongside the return type itself.

the examples narrow an optional value by testing its truthiness on purpose, so the lint that flags
such a test is off

```toml
[rules]
truthiness-test-of-none-union = "ignore"
```

## a returned predicate narrows the argument it is about

```by
def is_int(a: object):
    return a is int

def f(x: object):
    reveal_type(is_int)  # revealed: def is_int(a: object) -> a is int
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

A predicate that narrows to everything but a type is shown with the type parenthesized, since
`a is not None` would be the predicate negated.

```by
def present(a: int | None):
    return a is not None

def f(x: int | None):
    reveal_type(present)  # revealed: def present(a: int | None) -> a is (not None)
    if present(x):
        reveal_type(x)  # revealed: int
    else:
        reveal_type(x)  # revealed: None
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
    reveal_type(loaded)  # revealed: def loaded(h: Holder) -> h.data is str
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
    if is_int(x):  # error: [redundant-condition]
        reveal_type(x)  # revealed: object
    if yields_int(y):  # error: [redundant-condition]
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

## a predicate about another place returns a `bool`

A `TypeIs` is about the first argument wherever the type goes without its function, so a predicate
about any other place is typed as a `bool`. It narrows where the call is tested, and where a
variable that holds its result is.

```by
def second_is_int(a: object, b: object):
    return b is int

def f(x: object, y: object):
    reveal_type(second_is_int)  # revealed: def second_is_int(a: object, b: object) -> b is int
    if second_is_int(x, y):
        reveal_type(y)  # revealed: int
    result = second_is_int(x, y)
    reveal_type(result)  # revealed: bool
    if result:
        reveal_type(y)  # revealed: int
```

## a function stands in for a predicate only if it narrows as far

A call through a predicate's type narrows each place the predicate narrows, so a function that
narrows a different place, or less, cannot stand in for it, and a method that does so cannot
override it. One that narrows more can.

```by
from ty_extensions import static_assert
from ty_extensions._internal import CallableTypeOf, is_assignable_to

def second_is_int(a: object, b: object):
    return b is int

def second_is_str(a: object, b: object):
    return b is str

def first_is_int(a: object, b: object):
    return a is int

def plain(a: object, b: object) -> bool:
    return True

static_assert(not is_assignable_to(CallableTypeOf[second_is_str], CallableTypeOf[second_is_int]))
static_assert(not is_assignable_to(CallableTypeOf[first_is_int], CallableTypeOf[second_is_int]))
static_assert(not is_assignable_to(CallableTypeOf[plain], CallableTypeOf[second_is_int]))
static_assert(is_assignable_to(CallableTypeOf[second_is_int], CallableTypeOf[plain]))
```

So two of them in a list keep apart, and a call through either narrows nothing.

```by
def f(x: object, y: object):
    for check in [second_is_int, second_is_str]:
        if check(x, y):
            reveal_type(y)  # revealed: object
```

```by
from typing import override

class Base:
    def check(self, a: object, b: object):
        return b is int

class Child(Base):
    @override
    # error: [invalid-method-override]
    def check(self, a: object, b: object):
        return a is int

class Same(Base):
    @override
    def check(self, a: object, b: object):  # no diagnostic
        return b is int
```

A written `-> b is int` is the same claim, so either stands in for the other.

```by
from ty_extensions import static_assert
from ty_extensions._internal import CallableTypeOf, is_assignable_to

def written(a: object, b: object) -> b is int:
    return b is int

def recovered(a: object, b: object):
    return b is int

static_assert(is_assignable_to(CallableTypeOf[recovered], CallableTypeOf[written]))
static_assert(is_assignable_to(CallableTypeOf[written], CallableTypeOf[recovered]))
```

A side the function never returns on is one it says nothing false about, so one that only returns
`False` stands in for a predicate that narrows where it is true alone, though not for one that
narrows where it is false too.

```by
from typing import Literal
from ty_extensions import static_assert
from ty_extensions._internal import CallableTypeOf, is_assignable_to

def never_true(a: object, b: object) -> Literal[False]:
    return False

def int_and_flag(a: bool, b: object):
    return b is int and a

def is_int_or_not(a: object, b: object):
    return b is int

static_assert(is_assignable_to(CallableTypeOf[never_true], CallableTypeOf[int_and_flag]))
static_assert(not is_assignable_to(CallableTypeOf[never_true], CallableTypeOf[is_int_or_not]))
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
that argument was. `rebound` hands back `True` whatever it is given — the test is settled by the `1`
the body assigned — and reading that as a claim about the argument would narrow it to `Never`.

```by
def rebound(a: object):
    a = 1
    return a is int

def f(x: str):
    reveal_type(rebound)  # revealed: def rebound(a: object) -> True
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
    reveal_type(checked)  # revealed: def checked(a: A) -> A & protocol(let b: int)
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
    reveal_type(checked)  # revealed: def checked(o: Outer) -> Outer & protocol(let inner: protocol(let b: str))
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
    reveal_type(checked)  # revealed: def checked(o: Outer) -> Inner & protocol(let c: int)
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
    reveal_type(checked)  # revealed: def checked(a: A | None) -> A & protocol(let b: int)
```

## a body that returns nothing asserts what it establishes

`def check(a) -> asserts a` tells every caller that `a` is truthy once the call has returned. A
`def` that leaves its return type out and returns nothing says the same thing by asserting it, and
the call narrows its argument for the rest of the flow.

```by
def check(a: int | None):
    assert a

def f(x: int | None):
    check(x)
    reveal_type(x)  # revealed: int & not AlwaysFalsy
```

What is recovered is only what the body established, so a call intersects it with the argument it
actually passed.

```by
def require(a: int | None):
    if a is None:
        raise ValueError

def f(x: int | None):
    require(x)
    reveal_type(x)  # revealed: int
```

## every way out of the body has to agree

A call returns through any `return` it reaches as well as by falling off the end of the body, so
what is asserted is what all of them establish.

```by
def check(a: int | None):
    if a:
        return
    raise ValueError

def f(x: int | None):
    check(x)
    reveal_type(x)  # revealed: int & not AlwaysFalsy
```

A `return` the body reaches before establishing anything leaves nothing asserted.

```by
def check_unless(a: int | None, flag: bool):
    if flag:
        return
    assert a

def g(x: int | None):
    check_unless(x, True)
    reveal_type(x)  # revealed: int | None
```

## a failure the body catches establishes nothing

The body returns normally after an `AssertionError` it handles, whatever the argument was.

```by
def check(a: int | None):
    try:
        assert a
    except AssertionError:
        pass

def f(x: int | None):
    check(x)
    reveal_type(x)  # revealed: int | None
```

## a member the body establishes narrows the argument's

```by
class A:
    b: int | None = None

def check(a: A):
    assert a.b is not None

def f(x: A):
    check(x)
    reveal_type(x.b)  # revealed: int
```

## a parameter the body puts something else in asserts nothing

The body establishes something about what it assigned, not about the argument.

```by
def check(a: int | None, b: int | None):
    a = b
    assert a

def f(x: int | None, y: int | None):
    check(x, y)
    reveal_type(x)  # revealed: int | None
```

## a body that returns a value asserts nothing

The narrowing only reaches the code after a call written as a statement, and a call whose value is
used would lose it. So only a body with nothing to hand back is read as an assertion, the same as a
written `-> asserts`.

```by
def check(a: int | None):
    assert a
    return a

def f(x: int | None):
    check(x)
    reveal_type(x)  # revealed: int | None
```

`return None` hands back nothing, the same as a bare `return`.

```by
def check_nothing(a: int | None):
    assert a
    return None

def g(x: int | None):
    check_nothing(x)
    reveal_type(x)  # revealed: int & not AlwaysFalsy
```

## a coroutine asserts nothing until it is awaited

Calling an `async def` without awaiting it never runs the body.

```by
async def check(a: int | None):
    assert a

async def f(x: int | None):
    check(x)  # error: [unused-awaitable]
    reveal_type(x)  # revealed: int | None
```

## a `return` a `finally` suite can follow asserts nothing

The suite runs before the `return` hands control back: it can swallow the exception that was on its
way out, as here, and it can change what the caller reads. What a caller sees is the state after
that suite, so nothing is recovered from a body that returns through one.

```by
def check(a: int | None):
    try:
        assert a
    finally:
        return

def f(x: int | None):
    check(x)
    reveal_type(x)  # revealed: int | None
```

## a `break` that leaves a `finally` suite asserts nothing

A `break` or `continue` that leaves a `finally` suite discards the exception the suite was running
for, so the body carries on past an assertion that failed.

```by
def check(a: int | None):
    while True:
        try:
            assert a is not None
        finally:
            break

def f(x: int | None):
    check(x)
    reveal_type(x)  # revealed: int | None
```

`continue` leaves the suite the same way.

```by
def check_continue(a: int | None):
    for _ in range(3):
        try:
            assert a is not None
        finally:
            continue

def h(x: int | None):
    check_continue(x)
    reveal_type(x)  # revealed: int | None
```

A loop inside the suite is left by its own `break`, which leaves the suite running.

```by
def check_with_loop(a: int | None):
    try:
        assert a is not None
    finally:
        for _ in range(3):
            break

def g(x: int | None):
    check_with_loop(x)
    reveal_type(x)  # revealed: int
```

## a member the body asserts narrows the argument's member

```by
class A:
    b: int | None = None

def check(a: A):
    assert a.b is not None

def f(x: A):
    check(x)
    reveal_type(x.b)  # revealed: int
```

## an override is handed the assertion its base makes

A call through the base narrows on the strength of what the base's body established, and what it is
called on may be an instance of the subclass. So the assertion carries to the override, whose own
body has to establish it — an override that does not is reported where it is written.

```by
from typing import override

class Base:
    def ensure(self, x: int | None):
        assert x

class Silent(Base):
    @override
    # error: [unestablished-assertion-guard] "`x` is `int | None` where the body ends, but `Base.ensure`, which this overrides, asserts it is `not AlwaysFalsy`"
    def ensure(self, x: int | None):
        pass

def f(base: Base, silent: Silent, a: int | None, b: int | None):
    base.ensure(a)
    reveal_type(a)  # revealed: int & not AlwaysFalsy
    silent.ensure(b)
    reveal_type(b)  # revealed: int & not AlwaysFalsy
```

## an override that is not a `def` has to carry the assertion

An override is checked against what its base asserts by reading its body, and only a `def` has one.
Anything else that overrides the method — a `lambda`, a callable from elsewhere — is checked by its
type instead, which has to carry the same assertion.

```by
class Base:
    x: int | None = None

    def check(self):
        assert self.x is not None

class Lambda(Base):
    # error: [unestablished-assertion-guard] "`Lambda.check` is not a `def`, so nothing checks that it establishes what `Base.check`, which it overrides, asserts"
    check = lambda self: None
```

The same holds for one that comes from a base ahead of the asserting method's.

```by
class Mixin:
    check = lambda self: None

# error: [unestablished-assertion-guard] "`Mixin.check`, which `Combined` inherits, is not a `def`, so nothing checks that it establishes what `Base.check`, which it hides, asserts"
class Combined(Mixin, Base):
    pass
```

## an override that delegates to `super()` establishes what it delegates to

A call through `super()` is made on the method's own first argument, so it narrows that.

```by
from typing import override

class Base:
    x: int | None = None

    def check(self):
        assert self.x is not None

class Child(Base):
    @override
    def check(self):
        super().check()  # no diagnostic
        reveal_type(self.x)  # revealed: int
```

## a body that tests a variable it reads asserts it too

A name the body reads without binding it is a variable of an enclosing scope, which the caller can
see as well, so what the body establishes about it reaches the caller.

```by
def src() -> int | None:
    return 1

a = src()
b = src()

def check():
    assert a is not None

def f():
    reveal_type(a)  # revealed: int | None
    check()
    reveal_type(a)  # revealed: int
    reveal_type(b)  # revealed: int | None
```

## a variable of a function around the body is asserted the same way

```by
def src() -> int | None:
    return 1

def f():
    a = src()

    def check():
        assert a is not None

    check()
    reveal_type(a)  # revealed: int
```

## a local of the same name is a different variable

```by
def src() -> int | None:
    return 1

a = src()

def check():
    assert a is not None

def f():
    a = src()
    check()
    reveal_type(a)  # revealed: int | None

def g(a: int | None):
    check()
    reveal_type(a)  # revealed: int | None
```

## a member of a variable is asserted too

```by
class Holder:
    data: int | None = None

holder = Holder()

def check():
    assert holder.data is not None

def f():
    check()
    reveal_type(holder.data)  # revealed: int
```

## what a call in the body asserts, the body asserts

```by
def src() -> int | None:
    return 1

a = src()

def outer():
    inner()

def inner():
    assert a is not None

def f():
    outer()
    reveal_type(a)  # revealed: int
```

## a variable the body binds is not asserted

The assignment is what `a` holds afterwards, so the test says nothing the caller can rely on, and a
name the body binds without `global` is a local of its own.

```by
def src() -> int | None:
    return 1

a = src()

def rebinds():
    global a
    a = src()
    assert a is not None

def shadows():
    a = src()
    assert a is not None

def f():
    rebinds()
    reveal_type(a)  # revealed: int | None
    shadows()
    reveal_type(a)  # revealed: int | None
```

## a body that returns a value does not assert a variable

```by
def src() -> int | None:
    return 1

a = src()

def check():
    assert a is not None
    return 1

def f():
    check()
    reveal_type(a)  # revealed: int | None
```

## another module's variable is not this module's

`guard.by`:

```by
def src() -> int | None:
    return 1

a = src()

def check():
    assert a is not None
```

`main.by`:

```by
from guard import check, src

a = src()

def f():
    check()
    reveal_type(a)  # revealed: int | None
```

## an override makes the assertion on a variable too

```by
def src() -> int | None:
    return 1

a = src()

class Base:
    def check(self):
        assert a is not None

class Same(Base):
    override def check(self):
        assert a is not None

class Other(Base):
    # error: [unestablished-assertion-guard]
    override def check(self):
        pass
```

## a place satisfies a protocol with what its members were narrowed to

Reading `.a` under the test gives `int`, so the value can stand where that is all that is asked of
it, for as long as the narrowing holds.

```by
protocol P:
    let a: int

class A:
    init(let a: object)

def takes(p: P): ...

def f(a: A):
    if a.a is int:
        b: P = a  # no diagnostic
        takes(a)  # no diagnostic
        reveal_type(a)  # revealed: A
    # error: [invalid-assignment]
    c: P = a

def g(a: A) -> P:
    assert a.a is int
    return a  # no diagnostic
```

## a protocol that can write the member is not satisfied by a narrowing

The claim is what reading the member gives.

```by
protocol Writable:
    a: int

class A:
    init(var a: object)

def f(a: A):
    if a.a is int:
        # error: [invalid-assignment]
        b: Writable = a
```

## a place below the value counts as well

```by
protocol Inner:
    let b: int

protocol Outer:
    let a: Inner

class B:
    init(let b: object)

class A:
    init(let a: B)

def f(a: A):
    if a.a.b is int:
        c: Outer = a  # no diagnostic
        d: Inner = a.a  # no diagnostic
```

## assigning to the member ends what a test established

```by
protocol P:
    let a: int

class A:
    init(var a: object)

def f(a: A):
    if a.a is int:
        a.a = "s"
        # error: [invalid-assignment]
        b: P = a
```

## the same holds in a plain python file

`sound-types` recovers a signature in a `.py` file too, and the narrowing it recovers is the same.

```toml
[analysis]
sound-types = true

[rules]
truthiness-test-of-none-union = "ignore"
```

```py
def is_int(a: object):
    return isinstance(a, int)

def f(x: object):
    if is_int(x):
        reveal_type(x)  # revealed: int
```

A test on a member narrows that member, and a body that returns nothing asserts what it establishes,
about a parameter or about a variable it reads.

```py
class A:
    a: object

def member_is_int(a: A):
    return isinstance(a.a, int)

def assert_member_is_int(a: A):
    assert isinstance(a.a, int)

def assign_member(a: A):
    a.a = 1

def g(x: A, y: A, z: A):
    if member_is_int(x):
        reveal_type(x.a)  # revealed: int
    assert_member_is_int(y)
    reveal_type(y.a)  # revealed: int
    assign_member(z)
    reveal_type(z.a)  # revealed: object
```

```py
def src() -> int | None:
    return 1

a = src()

def check():
    assert a is not None

def h():
    reveal_type(a)  # revealed: int | None
    check()
    reveal_type(a)  # revealed: int
```

## a decorator typed with a `ParamSpec` keeps only the parameters and the return type

`Callable[P, R]` says the decorated function takes the parameters it took and returns what it
returned, and nothing about what its body does: the wrapper may catch the `AssertionError`, or test
something else. So no assertion survives the decorator, and no predicate except one typed as a
`TypeIs`, which comes back through `R`.

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true

[rules]
truthiness-test-of-none-union = "ignore"
```

```py
from typing import Callable

def wrap[**P, R](function: Callable[P, R]) -> Callable[P, R]:
    def wrapper(*args: P.args, **kwargs: P.kwargs) -> R:
        return function(*args, **kwargs)
    return wrapper

@wrap
def check(a: int | None):
    assert a is not None

@wrap
def first(a: object, b: object):
    return isinstance(a, int)

@wrap
def second(a: object, b: object):
    return isinstance(b, int)

def f(x: int | None, y: object, z: object):
    check(x)
    reveal_type(x)  # revealed: int | None
    if first(y, z):
        reveal_type(y)  # revealed: int
    if second(y, z):
        reveal_type(z)  # revealed: object
```

## a `TypeIs` a decorator declares narrows the first argument

A `TypeIs` the decorator declares itself narrows the first argument, as it says, whatever the
function's own body tested.

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

`lib.pyi`:

```pyi
from typing import Callable
from typing_extensions import TypeIs

def first_is_int[**P](function: Callable[P, object]) -> Callable[P, TypeIs[int]]: ...
```

`main.py`:

```py
from lib import first_is_int

@first_is_int
def tests_second(a: object, b: object):
    return isinstance(b, str)

def g(y: object, z: object):
    if tests_second(y, z):
        reveal_type(y)  # revealed: int
        reveal_type(z)  # revealed: object
```

## an override is checked against an assertion whichever override rules are on

Whether an override establishes what its base asserts is a question of soundness, so it is asked
even where the rules about overrides in general are off.

```toml
[rules]
invalid-method-override = "ignore"
invalid-attribute-override = "ignore"
```

```by
class Base:
    x: int | None = None

    def check(self):
        assert self.x is not None

def nothing(self: Base):
    pass

class Assigned(Base):
    # error: [unestablished-assertion-guard] "`Assigned.check` is a function defined elsewhere, so nothing checks that it establishes what `Base.check`, which it overrides, asserts"
    check = nothing

class Mixin:
    def check(self):
        pass

# error: [unestablished-assertion-guard] "`Mixin.check`, which `Combined` inherits, is not a method of a subclass of it, so nothing checks that it establishes what `Base.check`, which it hides, asserts"
class Combined(Mixin, Base):
    pass

# a member declared to be something that cannot be called never returns from a call, so it
# leaves nothing unestablished
class Disabled(Base):
    check: None = None  # no diagnostic

# an undeclared one may have a function stored in it later
class Unset(Base):
    check = None  # error: [unestablished-assertion-guard]

def flag() -> bool:
    return True

# one that only sometimes can be has to establish it where it can
class Sometimes(Base):
    if flag():
        # error: [unestablished-assertion-guard]
        check = lambda self: None
    else:
        check = None

class NotAsserting:
    def check(self) -> None:
        pass

class LambdaMixin:
    check = lambda self: None

# a member hides every later base's method, not only the nearest one's
# error: [unestablished-assertion-guard] "`LambdaMixin.check`, which `Hidden` inherits, is not a `def`, so nothing checks that it establishes what `Base.check`, which it hides, asserts"
class Hidden(LambdaMixin, NotAsserting, Base):
    pass

# a static method has no receiver to establish anything about
class Static(Base):
    @staticmethod
    # error: [unestablished-assertion-guard] "`Static.check` is not bound the way a method it overrides is, so nothing checks that it establishes what `Base.check`, which it overrides, asserts"
    def check():
        pass

class Typed(Base):
    # an `object` may well be a function
    # error: [unestablished-assertion-guard]
    check: object = lambda self: None

class Sub(Base):
    def check(self):
        super().check()

# `Sub.check`'s `super().check()` runs `LambdaMixin.check` here, which establishes nothing
# error: [unestablished-assertion-guard] "`LambdaMixin.check`, which `Cooperative` inherits, is not a `def`, so nothing checks that it establishes what `Base.check`, which it hides, asserts"
class Cooperative(Sub, LambdaMixin, Base):
    pass
```

A method bound differently from the one it overrides still has its body checked where the parameters
line up, past the receiver.

```by
class Validator:
    @staticmethod
    def check(value: int | None):
        assert value is not None

class Checked(Validator):
    def check(self, value: int | None):  # no diagnostic
        assert value is not None
```
