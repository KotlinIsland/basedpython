# sound types

python's [gradual guarantee] requires a type checker to fall back to a gradual type
(`Any` / `Unknown`) whenever an annotation is missing, even when a precise type could be
inferred. adding annotations must never introduce new errors, so an unannotated symbol has to
accept anything

in a fully typed project that trade is all cost and no benefit. it forces an annotation to be
written for something the checker already knows, and it silently swallows real mistakes

```python
def f(a=1):
    ...
f("nonsense")  # error: `a` is declared `int`
```

basedpython deliberately breaks the gradual guarantee and uses the precise type instead. this is
`analysis.sound-types`, which is on under the default [type checking
preset](../configuration.md#the-preset) and off under `ty-compatible`

```toml
[analysis]
# fall back to a gradual type wherever an annotation is missing
sound-types = false
```

an explicit annotation always wins over anything inferred by this option

## per-module configuration

the option is resolved per module, so a project migrating from a gradual checker can turn it off
and adopt it a directory at a time

```toml
[analysis]
sound-types = false

[[overrides]]
include = ["src/core/**"]

[overrides.analysis]
sound-types = true
```

the rule is that **the module declaring a construct governs how that construct's types are
inferred**, and consumers see the result whatever their own setting is. so a sound module's
signatures stay precise when a gradual module imports them

```python
# src/core/lib.py  — sound
def f(a=1) -> None: ...

# src/legacy/main.py  — gradual
from src.core.lib import f
f("wrong")  # error: `f` is declared in a sound module, so its signature is precise here too
```

and the reverse holds: a sound module importing a gradual one does not retroactively tighten it

```python
# src/legacy/lib.py  — gradual
def g(a=1) -> None: ...

# src/core/main.py  — sound
from src.legacy.lib import g
g("fine")  # no error: `g` is declared in a gradual module
```

this applies uniformly. a parameter default, an inherited override signature and a `ClassVar` are
governed by the module that declares them; a lambda or a collection literal by the module it is
written in; and a call's type-variable solution by the module declaring the callable, so a sound
module never re-interprets a gradual module's generics

## unannotated parameters

an unannotated parameter is not "some fixed type the checker failed to learn": it is a hole the call
site fills. so it opens an anonymous type parameter named after it — the same hole `some` spells by
hand — bounded by everything the function requires of it

naming the hole is what keeps what goes in connected to what comes back out. an unannotated identity
function really is one

```python
def ident(x):
    return x

reveal_type(ident)        # def ident(x) -> x
reveal_type(ident(1))     # Literal[1]
```

requirements come from three places, and the bound is their intersection. with none of them the
bound is gradual, which is exactly the `Unknown` an unannotated parameter has always had

### a default value

a default is a sample of what belongs there, so it bounds the hole by its promoted type — and
becomes the hole's [PEP 696] default, so a call that omits the argument still names a type

`None` is the exception, and it is the common one. it is the sentinel every optional parameter is
spelled with — it says the argument may be left out, not that `None` is the kind of thing that
belongs there — so it bounds nothing, and `def f(x=None)` still accepts whatever the body allows

```python
def f(a=1):
    reveal_type(a)  # a@f

reveal_type(f())    # Literal[1]
reveal_type(f(2))   # Literal[2]
f("x")              # error: invalid-argument-type
```

lambdas take the default's promoted type directly rather than opening a hole. a lambda body is a
single expression, so there is nothing else for the analysis below to read

```python
g = lambda a=1: a
g("x")    # error: invalid-argument-type
```

a `Callable` type context still takes priority over the default

```python
cb: Callable[[str], str] = lambda a="s": a  # `a` is `str`
```

### what the body does with it

a member the body reads is a member the argument has to have, and a method it calls has to be
callable the same way — same arity, same keywords. a member that is only read is read-only, so an
argument whose own attribute is typed more precisely still fits

```python
def f(x):
    x.foo()
    return 1
# def f(x: some protocol(def foo(self, /) -> object)) -> 1
```

a synthesized bound is spelled as the [inline protocol](inline-protocol.md) that would declare it,
so the recovered signature is something you could have written by hand

an operator, a subscript, iteration and calling the parameter itself are members like any other,
so each of those is a requirement too

```python
def f(s):
    a = s.rstrip("\r\n")
    b = s[:5]
# def f(s: some protocol(def __getitem__(self, slice[None, 5, None], /) -> Unknown; def rstrip(self, str, /) -> object))
```

iterating asks for two members at once — `__iter__`, and a `__next__` on whatever that hands back —
so what a loop body does with the loop variable is a requirement on what the argument *yields*

```python
def total(xs):
    for x in xs:
        x.bit_length()
# def total(xs: some protocol(def __iter__(self, /) -> protocol(def __next__(self, /) -> protocol(def bit_length(self, /) -> object))))
```

only the *left* operand of a binary operation carries the requirement. python reaches the right
operand's reflected dunder only when the left one returns `NotImplemented`, and which of the two an
operation takes is decided by the argument, so `2 * x` asks nothing of `x` — see
[a bound has to type the body it came from](#a-bound-has-to-type-the-body-it-came-from) for what
that then means for `x`

a member the body reached this way reads back as `Unknown` rather than `object`, because recording
a requirement is about what the *call site* has to supply and it should not change what the body
itself reads. so does a member the body named: the requirement is that it **exist**, and nothing
about naming it says what it holds. `object` would not describe such a value, it would forbid every
use of it — and that claim travels, because a member's type becomes the recovered return type of the
function that read it

a parameter the argument is forwarded into is a requirement too

```python
def takes_int(a: int) -> None: ...

def f(x):
    takes_int(x)

f("a")  # error: invalid-argument-type
```

and reading a value into somewhere that says what it holds is a requirement on that value. an
annotated assignment, a call argument and a declared return type are all such places, and each of
them constrains whichever value was read into it — a member, or the parameter itself

```python
def f(x):
    a: int = x.foo()
    return x
# def f(x: some protocol(def foo(self, /) -> int)) -> x

def g(x) -> str:
    return x
# def g(x: some str) -> str
```

an *inferred* return type is not one of them: it is read off the body, so it cannot also constrain
it. with nothing to say what a member holds, it only has to exist, and its value is `object`

what the body does with a member's value is a requirement on that member too, however deep it goes

```python
def f(x):
    a = x.foo()
    b = a.foo()
    assert b is int
# def f(x: some protocol(def foo(self, /) -> protocol(def foo(self, /) -> int)))
```

uses are recognised by *type*, not by spelling: a name that was reassigned, or narrowed, is no
longer the parameter, and what happens to it afterwards says nothing about the argument. a local
that a member's value was given to is read the same way: a name bound more than once cannot stand
for one value, and a use under a narrowing is about something narrower than the value it was bound
to

saying nothing is not the same as costing nothing. a narrowed *parameter* still carries the hole, so
the body goes on being checked against whatever bound the rest of it recovers — through a use no
requirement was ever built from, which is what
[a bound has to type the body it came from](#a-bound-has-to-type-the-body-it-came-from) rules out. so
a use like that takes the recovered bound away, exactly as the unnarrowed form of it would

```python
def bits(x):
    x.bit_length()
    if x:
        return 2 * x
    return 0
# def bits(x)
```

a narrowing the bound already implies is not one of these, which is what keeps the `assert` below
working. once `int` is `x`'s bound, `x` narrowed to an `int` *is* the hole, so the uses under such an
`assert` are recorded like any other — and what they record is then checked against the very bound
the `assert` put there

a parameter its own body rebinds keeps nothing at all, for the same reason. the reads above the
rebinding are not enough on their own: walking a linked structure asks only that the argument have
the member it walks along, so the rebinding lands on that member — whose value nothing described —
and the walk's next step would fail against the signature the function itself produced

```python
def deepest(tb):
    if tb.tb_next:
        tb = tb.tb_next
    return tb.tb_frame
# def deepest(tb)
```

### an `assert` at the top of the body

an `assert` there holds for every call that returns normally, so it is the author saying what they
were prepared to accept

```python
def f(x):
    assert isinstance(x, int)
    return x

f("a")  # error: invalid-argument-type
```

the same test inside an `if` says nothing — the author plainly meant the other branch to be
reachable

### a bound has to type the body it came from

every requirement above is read off the body, so the body is checked against the bound they add up
to. that makes one thing non-negotiable: a bound that the body's *own* code would fail against is
worse than no bound at all, because it makes the checker report an error in the very function it
claims to have understood

so a use the analysis cannot state does not get passed over — it takes the bound away

```python
def area(r):
    a = r.bit_length()
    return 2 * r
# def area(r)
```

reading `r.bit_length()` on its own asks for a protocol. `2 * r` two lines later is exactly what such
a protocol cannot answer: python reaches `r`'s reflected dunder only when `int.__mul__` returns
`NotImplemented`, and which of the two routes an operation takes is decided by the argument, so there
is nothing here to ask of `r`. keeping the protocol would make `area`'s own body stop compiling
against `area`'s own signature, so the protocol goes

the same rule applies one member deep: a use nothing can be said about takes away only the value it
is about. `x.foo` still has to be there, and still has to be callable the way the body called it

```python
def held(x):
    return 1 + x.foo()
# def held(x: some protocol(def foo(self, /) -> Unknown))
```

### what is left out

the uses that cannot be stated are the ones where python's own answer is a disjunction, or a shape an
inline protocol has no way to write:

- an operand on the *right* of an operation whose left operand does not take anything, as above
- `in`, which runs through `__contains__`, `__iter__` *or* `__getitem__` on the container
- `await`, `async for`, `with`, `raise`, `del x.a`, `match`, and `**x` in a call
- writing a member rather than reading one — `x.a = 1`
- a call whose arguments are splatted, since no fixed parameter list says how many there are
- an argument whose parameter cannot be worked out, or is one this cannot write down
- anything at all written on a name a test narrowed, unless the position takes anything: the branch
    was written because the author meant the other one to be reachable, so holding every argument to
    what this one does would reject the very calls the test exists for

the other side of that rule is what keeps most code unaffected: wherever a position accepts `object`
it accepts whatever bound the body recovers, so nothing has to be recorded and nothing is lost.
printing a value, formatting one, reading one for its truth, looking a key up in a mapping and
`"%s" % x` are all positions like that

a forwarded type that mentions a type variable is left out too: it is bound to the callee's own
scope, and the same rule stops two functions that forward into each other from each defining the
other

a value the body reached *through* a parameter is left out on the same grounds. reading a member
off one leaves the shape this analysis invented for that member, so requiring a method to accept it
would be requiring something of the very type being written

```python
class Inner:
    def b(self, other: int) -> None: ...

class Outer:
    a: Inner

def f(x):
    x.a.b(x.a)

f(Outer())  # ok — nothing was required of `b`'s parameter
```

requirements that cannot all hold fall back to gradual as well. a bound of `Never` would report the
contradiction at every call site and never where it lives

## unannotated return types

a function with no return annotation returns what its body returns: the union of every `return`
expression, plus `None` when control can also fall off the end

```python
def f():
    return 1

reveal_type(f())    # Literal[1]
```

a body with nothing in it — a stub, a protocol member, an `abstractmethod` — returns `None`, which
is what running it would do. a body that always raises returns `Never`

```python
class X(Protocol):
    def f(self): ...   # (self) -> None
```

a generator returns a generator: its `yield` expressions supply the yield type and its `return`
statements the third type argument. the send type is the one thing the body does not determine, so
it stays gradual

```python
def gen():
    yield 1
    return "done"

reveal_type(gen())  # GeneratorType[Literal[1], Unknown, Literal["done"]]
```

a function that calls itself is read from the returns that do not recurse, so `fact` below returns
`int`. a body that wraps its *own* result has no type it settles on — each reading is a constructor
deeper than the last — and the part that grows is marked `Divergent` rather than chased

```python
def fact(n: int):
    if n < 2:
        return 1
    return n * fact(n - 1)

reveal_type(fact(5))    # int

def recur(a):
    return [recur(b) for b in a]

reveal_type(recur([]))  # list[Divergent]
```

### a written-out `-> None`

because `None` is now what a `def` means when it says nothing, writing it out adds a word without
adding information, and `redundant-return-annotation` says so

```python
def f() -> None:  # warning: redundant `-> None`
    print("hi")
```

the question is only ever whether deleting the two words changes anything. where the type would
come from instead — the body, an overridden base, a sibling overload group — decides nothing, so
what is left unreported is exactly the `def`s that would return something other than `None`

```python
def f() -> None:  # ok — the body returns `Never`
    raise ValueError

def g() -> None:  # ok — a generator returns a generator
    yield 1
```

an override is the case where this is easiest to get wrong. deleting the annotation makes it
inherit the base's return type, so whether `-> None` is redundant is decided by the base

```python
class Base:
    def m(self) -> int | None: ...

class Sub(Base):
    def m(self) -> None:  # ok — without it, `m` would return `int | None`
        print("hi")
```

with neither `sound-types` nor `infer-unannotated-signatures` on, nothing is reported: there
`-> None` is load-bearing, since dropping it widens the return type to `Unknown`

## a whole signature, recovered

the two halves compose. because the parameter is named, the return type can refer back to it, and an
unannotated body gets the signature it would have been given by hand

```python
def f(x="asdf"):
    return x.startswith("foo")
# def f(x: some str = "asdf") -> x.startswith("foo")

reveal_type(f("foobar"))  # True
```

## unannotated overrides

an unannotated method inherits the parameter and return types of the method it overrides. the
lookup starts *after* the class itself — the same walk `super()` performs — so it finds what is
being overridden rather than the method being defined

```python
class Base:
    def m(self, a: int, b: str = "x") -> bytes: ...

class Sub(Base):
    def m(self, a, b="y"):
        reveal_type(a)  # int
        reveal_type(b)  # str
        return b""

Sub().m("nope")  # error: invalid-argument-type
```

`Protocol` members and `abstractmethod` declarations are ordinary base methods for this purpose

a method that overrides nothing falls back to the analysis above. the base signature is also ignored
when it mentions a type variable (including an implicit `Self`), because those are bound to the base
method's own scope and copying them across would silently rebind them

## bare `ClassVar`

a bare `ClassVar` uses its inferred type. without this, adding `ClassVar` — a strengthening of
intent — would *degrade* the type relative to writing nothing at all

```python
class C:
    x: ClassVar = 1   # int   (was `Unknown | Literal[1]`)
    y = 1             # int
```

## empty collection literals

an empty collection literal has element type `Never`, so `first([])` solves `T` to `Never`
instead of leaking `Unknown`. a non-empty literal is unaffected

a type variable a call leaves *unsolved* is a separate matter, and is covered by
[precise unsolved type variables](precise-unsolved-typevars.md), which is on by default

## not covered

these are known gradual-guarantee costs that `sound-types` does **not** currently address

- **a use the body analysis cannot read** contributes nothing: `def f(x): return 1 in x` leaves `x`
    gradual, because `in` runs through `__contains__`, `__iter__` *or* `__getitem__` and asking for
    any one of the three would demand something the body never needed. an `async for`, a `with` and
    an operation whose left operand is not the parameter are left out for the same kind of reason
- **an operation gives way to what the program states**: where a default value, an `assert` or a
    forwarded parameter type rules the operation out, the statement wins and the operation is
    reported in the body where it lives rather than at every call site. `def g(x=0): x + "foo"` is
    an error in `g`
- **a call recorded twice needs one signature**: two calls of the same member union position by
    position, so `m.group("a")` and `m.group("b")` ask for a `group` that takes `str`. two calls of
    different *shape* would need an overload, which cannot be written here, so such a member
    degrades to asking only that it exist and be callable
- **a call site whose argument is itself an unsolved hole** is where most of the remaining noise
    lives: the callee's requirement cannot be propagated onto the caller's own hole, because it is
    a shape this analysis invented, so the forward is reported instead
- **`*args` / `**kwargs` do not open a hole**: one type parameter cannot name a run of arguments, so
    they stay `tuple[Unknown, ...]` / `dict[str, Unknown]`, and a `target(**kwargs)` forward is
    entirely unchecked
- **a lambda parameter does not open a hole**, so `lambda x: x` is not inferred as the identity
    function the way `def` is
- **unannotated function decorators erase the signature**: under `@deco` where `deco` is
    unannotated, the decorated function becomes `Unknown` and its call sites go unchecked. ty already
    preserves the decorated object through an unannotated *class* decorator; the function case should
    follow
- **singleton promotion in collection literals**: `[None]` is `list[None | Unknown]`. fluid
    specializations now cover what this promotion was for
- **implicit attribute collections across methods**: `self.items = []` in `__init__` plus
    `self.items.append(v)` elsewhere gives `list[Unknown]`; fluid specializations stop at the scope
    boundary
- **`Any` from typeshed and third-party stubs** (`json.loads`) stays gradual

## known rough edges

- a collection default goes through literal promotion rather than the collection-literal path, so
    `def g(a=[])` is `list[Unknown]` and `def h(a=())` is `tuple[()]`

[gradual guarantee]: https://typing.python.org/en/latest/spec/concepts.html#the-gradual-guarantee
[pep 696]: https://peps.python.org/pep-0696/
