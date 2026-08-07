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
f("nonsense")  # no error: `a` is gradual, so anything goes
```

the `analysis.sound-types` option deliberately breaks the gradual guarantee and uses the precise
type instead

```toml
[analysis]
sound-types = true
```

an explicit annotation always wins over anything inferred by this option

## per-module configuration

the option is resolved per module, so it can be adopted incrementally. use an override to enable it
for part of a project

```toml
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

a parameter the argument is forwarded into is a requirement too

```python
def takes_int(a: int) -> None: ...

def f(x):
    takes_int(x)

f("a")  # error: invalid-argument-type
```

and reading a member into somewhere that says what it holds constrains that member, not just the
parameter. an annotated assignment, a call argument and a declared return type are all such places

```python
def f(x):
    a: int = x.foo()
    return x
# def f(x: some protocol(def foo(self, /) -> int)) -> x
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

### what is left out

nothing is invented from a use that was not understood, so a body keeps type-checking exactly as it
did and its call sites stay unchecked. a forwarded type that mentions a type variable is left out
too: it is bound to the callee's own scope, and the same rule stops two functions that forward into
each other from each defining the other

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

- **a use the body analysis cannot read** contributes nothing: `def f(x): return x + 1` leaves `x`
    gradual. only attribute reads, method calls, forwarding into an annotated parameter and a
    top-level `assert` are read. operators, subscripting, iteration and calling the parameter itself
    are not
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
