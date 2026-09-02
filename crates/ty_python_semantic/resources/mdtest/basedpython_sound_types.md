# basedpython: sound types

python's gradual guarantee requires a checker to fall back to a gradual type whenever an annotation
is missing, even when a precise type could be inferred. in a fully typed project that is pure
boilerplate — it forces an annotation for something the checker already knows.

the `analysis.sound-types` option deliberately breaks that guarantee and uses the precise type
instead. an explicit annotation always wins over anything inferred here.

this is a basedpython enhancement that also applies to plain python files.

## parameter defaults

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

an unannotated parameter opens an anonymous type parameter named after it — a `some` hole nobody
wrote. a default value bounds that hole by its promoted type:

```py
def f(a=1):
    reveal_type(a)  # revealed: a@f

def g(a="s", b=True):
    reveal_type(a)  # revealed: a@g
    reveal_type(b)  # revealed: b@g
```

the signature is checked at call sites: an incompatible argument is now an error:

```py
f(2)  # ok
f("x")  # error: [invalid-argument-type]

g("hello", False)  # ok
g(1, True)  # error: [invalid-argument-type]
```

a parameter with no default opens a hole too; with nothing to go on, it is bounded gradually and so
still accepts anything:

```py
def h(a, b=1):
    reveal_type(a)  # revealed: a@h
    reveal_type(b)  # revealed: b@h

h("anything", 2)  # ok
```

an explicit annotation always wins over the default:

```py
def annotated(a: str = "s"):
    reveal_type(a)  # revealed: str

annotated("t")  # ok
annotated(1)  # error: [invalid-argument-type]
```

## a `None` default is a sentinel, not a sample

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = true
```

`x=None` is how an optional parameter is spelled. the default says the argument may be left out, not
that `None` is the kind of thing that belongs there, so it bounds nothing:

```py
def greet(name=None):
    if name is None:
        name = "world"
    return "hello " + name

greet()  # ok
greet("bob")  # ok
```

an unannotated third-party signature is the common case — nothing in the body has to mention it:

```py
def safe_dump(data, stream=None, **kwds): ...

safe_dump({}, open("f"))  # ok
```

every other default still bounds the hole:

```py
def f(x=0):
    return x

f(1)  # ok
f("s")  # error: [invalid-argument-type]
```

## anonymous type parameters

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

naming the hole is what keeps what a call passes in connected to what it gets back. an unannotated
identity function really is one:

```py
def ident(x):
    return x

reveal_type(ident)  # revealed: def ident(x) -> x
reveal_type(ident(1))  # revealed: Literal[1]
reveal_type(ident("a"))  # revealed: Literal["a"]
```

the hole's [PEP 696](https://peps.python.org/pep-0696/) default is the parameter's own default
value, so a call that omits the argument still names a type rather than leaving the hole unsolved:

```py
def defaulted(x=1):
    return x

reveal_type(defaulted())  # revealed: Literal[1]
reveal_type(defaulted(2))  # revealed: Literal[2]
```

## inferred parameter types

```toml
[environment]
python-version = "3.13"

[analysis]
sound-types = true
```

what a body does with a parameter is a requirement on the argument. the hole's bound is everything
the body requires of it.

### a method call

```py
class A:
    def foo(self) -> int:
        return 1

def f(x):
    x.foo()
    return 1

reveal_type(f)  # revealed: def f(x: some <Protocol with members 'foo'>) -> Literal[1]

f(A())  # ok
# error: [invalid-argument-type] "Argument type `Literal[1]` does not satisfy `<Protocol with members 'foo'>`, inferred for parameter `x`"
f(1)
```

### an attribute read

a member that is only read is a read-only member, so an argument whose own attribute is typed more
precisely still fits

```py
class A:
    name: str = ""

def f(x):
    return x.name

reveal_type(f(A()))  # revealed: Unknown

f(A())  # ok
f(1)  # error: [invalid-argument-type]
```

### the call decides the member's shape

a member has to be callable the way the body calls it — same arity, same keywords

```py
class Exact:
    def go(self, a: int, *, flag: bool = False) -> None: ...

class WrongArity:
    def go(self) -> None: ...

class WrongKeyword:
    def go(self, a: int, *, other: bool = False) -> None: ...

def f(x):
    x.go(1, flag=True)

f(Exact())  # ok
f(WrongArity())  # error: [invalid-argument-type]
f(WrongKeyword())  # error: [invalid-argument-type]
```

### several uses

```py
class A:
    name: str = ""

    def foo(self) -> int:
        return 1

def f(x):
    x.foo()
    return x.name

f(A())  # ok
# error: [invalid-argument-type] "Argument type `Literal[1]` does not satisfy `<Protocol with members 'foo', 'name'>`, inferred for parameter `x`"
f(1)
```

### a subscript

`x[k]` is a call on `__getitem__`, which is a member like any other, so subscripting a parameter is
a requirement on the argument in the same way reading an attribute off it is

```by
def f(s):
    return s[:5]

# revealed: def f(s: some protocol(def __getitem__(self, slice[None, 5, None], /) -> Unknown)) -> Unknown
reveal_type(f)

f("hello")  # ok
f(1)  # error: [invalid-argument-type]
```

`x[k] = v` asks for `__setitem__` instead, taking the key and the value

```by
def store(d):
    d["k"] = 1

reveal_type(store)  # revealed: def store(d: some protocol(def __setitem__(self, str, int, /) -> Unknown))

store({"k": 1})  # ok
store([])  # error: [invalid-argument-type]
```

### an operator

an operator is a call on the left operand's dunder

```by
def sub(n):
    return n - 1

reveal_type(sub)  # revealed: def sub(n: some protocol(def __sub__(self, int, /) -> Unknown)) -> Unknown

sub(5)  # ok
sub("nope")  # error: [invalid-argument-type]
```

the unary operators and the ordering comparisons are read the same way. `==`, `!=` and the identity
tests are not, because `object` answers those itself and they require nothing

```by
def measure(v):
    return -v

reveal_type(measure)  # revealed: def measure(v: some protocol(def __neg__(self, /) -> Unknown)) -> Unknown

measure(5)  # ok
measure(None)  # error: [invalid-argument-type]

def ordered(v):
    return v < 0

reveal_type(ordered)  # revealed: def ordered(v: some protocol(def __lt__(self, int, /) -> Unknown)) -> Unknown

ordered(5)  # ok
ordered(None)  # error: [invalid-argument-type]

def identical(v):
    return v == 1

identical(object())  # ok
```

the value an operator produces is not itself a value this tracks, so what the body goes on to do
with `n - 1` is no requirement on what `n`'s `__sub__` returns

```by
def stepped(n):
    return (n - 1).bit_length()

reveal_type(stepped)  # revealed: def stepped(n: some protocol(def __sub__(self, int, /) -> Unknown)) -> Unknown
```

### iteration

iterating a parameter asks for `__iter__`, and for a `__next__` on whatever that hands back. the two
together are what makes an *element* a value in its own right, so what the loop body does with the
loop variable is a requirement on what the argument yields rather than on the argument

```by
def total(xs):
    for x in xs:
        x.bit_length()

# revealed: def total(xs: some protocol(def __iter__(self, /) -> protocol(def __next__(self, /) -> protocol(def bit_length(self, /) -> Unknown))))
reveal_type(total)

total([1, 2])  # ok
total(["a"])  # error: [invalid-argument-type]
total(1)  # error: [invalid-argument-type]
```

a comprehension, a splatted argument and a destructuring assignment all iterate too

```by
def comprehended(xs):
    return [x for x in xs]

def splatted(xs):
    print(*xs)

def destructured(xs):
    a, b = xs

comprehended([1])  # ok
comprehended(1)  # error: [invalid-argument-type]
splatted([1])  # ok
splatted(1)  # error: [invalid-argument-type]
destructured([1, 2])  # ok
destructured(1)  # error: [invalid-argument-type]
```

### calling the parameter itself

calling a parameter asks for a `__call__` shaped like the call, which is what a function has

```by
def apply(fn):
    return fn(1)

reveal_type(apply)  # revealed: def apply(fn: some protocol(def __call__(self, int, /) -> Unknown)) -> Unknown

def takes_int(a: int) -> str:
    return ""

apply(takes_int)  # ok
apply(1)  # error: [invalid-argument-type]
```

### a member called twice takes both

each call is a separate requirement and all of them have to hold, so the member has to accept every
argument any of them passed. a parameter is contravariant, so the calls combine by unioning position
by position

```by
class Two:
    def group(self, which: str) -> int:
        return 1

class One:
    def group(self, which: "indent") -> int:
        return 1

def f(m):
    m.group("indent")
    m.group("source")

reveal_type(f)  # revealed: def f(m: some protocol(def group(self, str, /) -> Unknown))

f(Two())  # ok
f(One())  # error: [invalid-argument-type]
```

### calls that do not agree on their shape

two calls of different arity need an overload, which cannot be written here. such a member degrades
to asking only that it exist and be callable

```by
def f(m):
    m.group("indent")
    m.group(1, 2)

reveal_type(f)  # revealed: def f(m: some protocol(def group(self, /, *args: Any, **kwargs: Any) -> Unknown))
```

### a member nothing was required of reads back gradually

recording a requirement is about what the *call site* has to supply, and it does not change what the
body itself reads. `x[k]` and `x - 1` read as `Unknown` whether or not anything was recorded, which
is what keeps a body that used to check from acquiring errors about a value nothing learned anything
about

a member the body *named* reads back the same way. the requirement is that the member **exist** —
nothing about it says what it holds, and `object` would not describe that value, it would forbid
every use of it. that is a stronger claim than the source made, and it travels: a member's type
becomes the recovered *return* type of the function that read it, so `config = yaml.safe_load(fp)`
came back as an `object` and `config["plugins"]` was an error no annotation could take back

```py
def f(x):
    reveal_type(x[0])  # revealed: Unknown
    reveal_type(x - 1)  # revealed: Unknown
    reveal_type(x.name)  # revealed: Unknown
```

### a requirement another requirement already meets is left out

`int` has the `__mul__` the body needs, so intersecting the two would only cost the body the `int`
it could otherwise read back

```py
def twice(n=1):
    return n * 2

reveal_type(twice)  # revealed: def twice(n: some int = 1) -> int
```

### a declared place says what the member's value has to be

reading a member into somewhere that says what it holds is a requirement on that member, not just on
the parameter

```py
from typing import Protocol

def f(x):
    a: int = x.foo()
    return x

reveal_type(f)  # revealed: def f(x: some <Protocol with members 'foo'>) -> x

class Str(Protocol):
    def foo(self) -> str: ...

class Bool(Protocol):
    def foo(self) -> bool: ...

def m(str_foo: Str, bool_foo: Bool):
    f(str_foo)  # error: [invalid-argument-type]
    f(bool_foo)  # ok
    reveal_type(f(bool_foo))  # revealed: Bool
```

a call argument and a declared return type are declared places too

```py
class HasName:
    name: str = ""

class HasIntName:
    name: int = 0

def takes_str(s: str) -> None: ...
def forwards_member(x):
    takes_str(x.name)

forwards_member(HasName())  # ok
forwards_member(HasIntName())  # error: [invalid-argument-type]

def returns_member(x) -> str:
    return x.name

returns_member(HasName())  # ok
returns_member(HasIntName())  # error: [invalid-argument-type]
```

an *inferred* return type is not a declared place — it is read off the body, so it cannot also
constrain it

```py
def unannotated(x):
    return x.name

unannotated(HasName())  # ok
unannotated(HasIntName())  # ok
```

### a parameter it is forwarded into

```py
def takes_int(a: int) -> None: ...
def f(x):
    takes_int(x)

reveal_type(f)  # revealed: def f(x: some int)

f(1)  # ok
f("a")  # error: [invalid-argument-type]
```

### a forwarded type that mentions a type variable says nothing

it is bound to the callee's own scope, so copying it here would rebind it. the same rule keeps two
functions that forward into each other from each defining the other

```py
def generic[T](a: T) -> T:
    return a

def f(x):
    generic(x)

f(1)  # ok
f("anything")  # ok

def g(x):
    h(x)

def h(y):
    g(y)

g(1)  # ok
h("anything")  # ok
```

### an `assert` at the top of the body

an `assert` holds for every call that returns normally, so it says what the author was prepared to
accept

```py
def f(x):
    assert isinstance(x, int)
    return x

reveal_type(f(1))  # revealed: Literal[1]

f(1)  # ok
f("a")  # error: [invalid-argument-type]
```

### the same test inside a branch says nothing

the author plainly meant the other branch to be reachable

```py
def f(x):
    if isinstance(x, int):
        return x
    return None

reveal_type(f("anything"))  # revealed: None

f("anything")  # ok
```

### an `assert` about a member's value

`a = x.foo()` followed by an `assert` about `a` says what `x.foo()` has to return, just as plainly
as `a: int = x.foo()` does

```by
def f(x):
    a = x.foo()
    assert a is int

reveal_type(f)  # revealed: def f(x: some protocol(def foo(self, /) -> int))
```

### what a member's value is used for is a requirement on it

what the body does with a value read off a parameter says what that value has to be, however deep it
goes: a member read off it is a member the value it came from has to have

```by
def f(x):
    a = x.foo()
    b = a.foo()
    c = b.foo()
    assert c is int

# revealed: def f(x: some protocol(def foo(self, /) -> protocol(def foo(self, /) -> protocol(def foo(self, /) -> int))))
reveal_type(f)
```

### a chain against real classes

```py
class Deep:
    def foo(self) -> int:
        return 1

class Mid:
    def foo(self) -> Deep:
        return Deep()

class Top:
    def foo(self) -> Mid:
        return Mid()

def f(x):
    a = x.foo()
    b = a.foo()
    c = b.foo()
    assert isinstance(c, int)

f(Top())  # ok
f(Mid())  # error: [invalid-argument-type]
f(Deep())  # error: [invalid-argument-type]
```

### a chain returned answers in the caller's own types

the bound the chain builds is a protocol, but what the body returns is the *call*, so a call site
answers with the class the argument's own method returns rather than with the protocol the bound had
to spell. keeping a call symbolic is a basedpython-only reading, so this needs a `.by` file.

```by
class Deep:
    def foo(self) -> int:
        return 1

class Mid:
    def foo(self) -> Deep:
        return Deep()

class Top:
    def foo(self) -> Mid:
        return Mid()

def f(x):
    a = x.foo()
    b = a.foo()
    c = b.foo()
    assert isinstance(c, int)
    return b

reveal_type(f(Top()))  # revealed: Deep
```

### a chain a declared place terminates

the last link does not have to be an `assert`; anywhere that says what it holds will do

```py
class Deep:
    def foo(self) -> int:
        return 1

class Mid:
    def foo(self) -> Deep:
        return Deep()

def takes_int(a: int) -> None: ...
def f(x):
    a = x.foo()
    b: int = a.foo()

def g(x):
    a = x.foo()
    takes_int(a.foo())

f(Mid())  # ok
f(Deep())  # error: [invalid-argument-type]
g(Mid())  # ok
g(Deep())  # error: [invalid-argument-type]
```

### a chain through a reassigned local says nothing

which of a name's values a later use is about is not a question this can answer. so the chain stops
there: the member still has to be there, and what it hands back is a value nothing here can say
anything about

```by
class A:
    def foo(self) -> str:
        return ""

def f(x, flag):
    a = x.foo()
    if flag:
        a = 1
    assert a is int

reveal_type(f)  # revealed: def f(x: some protocol(def foo(self, /) -> Unknown), flag)

f(A(), True)  # ok
```

### a narrowed local says nothing either

a use under a narrowing is about something narrower than the value the name was bound to, and
requires nothing of that value — the author plainly meant the other branch to be reachable. a branch
that narrows nothing is still a use

```by
def guarded(x):
    a = x.foo()
    if a is int:
        a.bit_length()

reveal_type(guarded)  # revealed: def guarded(x: some protocol(def foo(self, /) -> Unknown))

def returned(x):
    a = x.foo()
    if a is not int:
        return None
    a.bit_length()

reveal_type(returned)  # revealed: def returned(x: some protocol(def foo(self, /) -> Unknown))

def branched(x, flag):
    a = x.foo()
    if flag:
        a.bar()

# revealed: def branched(x: some protocol(def foo(self, /) -> protocol(def bar(self, /) -> Unknown)), flag)
reveal_type(branched)
```

### a use this analysis cannot read leaves the parameter gradual

nothing is invented from a use that was not understood, so a body keeps type-checking exactly as it
did and its call sites stay unchecked

`in` is such a use where the container decides what it takes. it runs through `__contains__`,
`__iter__` *or* `__getitem__` on the right-hand operand, and asking for any one of the three would
demand something the body never needed

```py
def f(x):
    return 1 in x

f("anything")  # ok
```

an operation whose *left* operand is not the parameter is another. python only reaches the right
operand's reflected dunder when the left operand's own returns `NotImplemented`, and which of the
two routes it takes is decided by the argument: `"%s" % attr` runs entirely through `str.__mod__`,
and requiring `attr` to have `__rmod__` would reject every `str`

```py
def g(x):
    return "%s" % x

g("anything")  # ok
```

### a bound has to type the body it was read off

every requirement above is read off the body, so the body is checked against the bound they add up
to. a use of the *same* parameter that no requirement can state is what makes that fail: the bound
is built without that use and then checked against it anyway, and the function's own code stops
fitting the signature the function itself produced

so a use like that takes the bound away rather than being passed over. reading `r.bit_length()` asks
for a protocol, and `2 * r` two lines later is exactly the operation such a protocol cannot answer,
so `r` keeps nothing

```py
def area(r):
    a = r.bit_length()
    return 2 * r

reveal_type(area)  # revealed: def area(r) -> Unknown
area("anything")  # ok
```

the same body without that line keeps everything it read

```py
def bits(r):
    a = r.bit_length()
    return r

# revealed: def bits(r: some <Protocol with members 'bit_length'>) -> r
reveal_type(bits)
```

### a use that cannot be stated only takes away what it is about

a use of a *member's* value says nothing about the member itself. `x.foo` still has to be there and
still has to be callable the way the body called it; what it hands back is the part nothing can be
said about, so that part reads back as gradual

```py
def held(x):
    return 1 + x.foo()

# revealed: def held(x: some <Protocol with members 'foo'>) -> Unknown
reveal_type(held)

held(1)  # error: [invalid-argument-type]
```

### a narrowed use takes the bound away too

a name a test narrowed stands for something narrower than the argument, so nothing done with it is a
requirement on the argument. it is still the same argument underneath, though, and the type it now
has still mentions the hole — so the body goes on being checked against whatever bound the rest of
it recovers, through a use no requirement was ever built from. that is the failure above reached by
a route the walk does not see, and it is closed the same way

```py
def bits(x):
    x.bit_length()
    if x:
        return 2 * x
    return 0

reveal_type(bits)  # revealed: def bits(x) -> int
bits("anything")  # ok
```

a member read under a narrowing goes the same way, and for a reason of its own: the branch was
written because the author meant the other one to be reachable, so requiring of every argument what
this one does would reject the very calls the test exists for

```py
def branch(x):
    x.foo()
    if x:
        return x.bar()
    return 0

reveal_type(branch)  # revealed: def branch(x) -> Unknown | Literal[0]
```

### a narrowing the bound already implies is no narrowing

`assert isinstance(x, int)` puts `int` into the bound, and from there `x` narrowed to an `int` is
the hole itself. so the uses below such an `assert` are recorded like any other — and what they
record is checked against the very bound the `assert` put there, which is what makes recording them
safe

```py
def f(x):
    assert isinstance(x, int)
    return x + 1

reveal_type(f)  # revealed: def f(x: some int) -> int

f(1)  # ok
f("a")  # error: [invalid-argument-type]
```

### a narrowing costs only a bound the body itself recovered

what it takes away is what the walk read off the body, so where the walk read nothing there is
nothing for the narrowed use to fail against and nothing to take. that is what lets an `assert` be
read from inside its own test: `isinstance(proto, int)` narrows `proto` for the arm beside it, and
nothing else in this body asks anything of `proto` at all

```py
def opcode(proto):
    assert isinstance(proto, int) and proto <= 5

reveal_type(opcode)  # revealed: def opcode(proto: some int)

opcode(1)  # ok
opcode("a")  # error: [invalid-argument-type]
```

### the uses that cannot be stated

each of these asks something of the parameter that no requirement here can write down, so each one
leaves the parameter gradual however much else its body said

writing a member, rather than reading one:

```py
def written(x):
    x.foo()
    x.other = 1

reveal_type(written)  # revealed: def written(x)
```

a call whose arguments are splatted, since no fixed parameter list says how many of them there are:

```py
def spread(x, args):
    x.foo(*args)
    x.bar()

# revealed: def spread(x, args: some <Protocol with members '__iter__'>)
reveal_type(spread)
```

and the statements that ask for a shape of their own — `raise` for a `BaseException`, `with` for a
pair of context-manager methods:

```py
def thrown(x):
    x.foo()
    raise x

reveal_type(thrown)  # revealed: def thrown(x) -> Never

def entered(x):
    x.foo()
    with x:
        pass

reveal_type(entered)  # revealed: def entered(x)
```

### a position that takes anything asks nothing

the other side of the same rule: where a position accepts `object` it accepts whatever bound the
rest of the body recovers, so the body goes on checking and the bound stays

that covers a great deal of ordinary python — a value printed or formatted, one read for its truth,
a key looked up in a mapping, an argument to an overloaded callee every reading of which takes
anything

```py
def shown(x):
    x.foo()
    print(x)
    print(f"{x}")
    print(str(x))
    return x or 0

# revealed: def shown(x: some <Protocol with members 'foo'>) -> (x & ~AlwaysFalsy) | Literal[0]
reveal_type(shown)

def keyed(x, d: dict[str, int]):
    x.foo()
    return d[x] if x in d else 0

# revealed: def keyed(x: some <Protocol with members 'foo'>, d: dict[str, int]) -> int
reveal_type(keyed)
```

`"%s" % x` is the same thing read through an operator: `str.__mod__` takes anything, so the operand
written on its right is asked nothing and keeps whatever else the body said about it

```py
def formatted(x):
    x.foo()
    return "%s" % x

# revealed: def formatted(x: some <Protocol with members 'foo'>) -> str
reveal_type(formatted)
```

a narrowed value is still a value, and a position like this takes one of those too, so a narrowing
whose uses all sit in positions like this costs nothing

```py
def guarded(x):
    x.foo()
    if x:
        print(x)
        print(f"{x}")
        print("%s" % x)
    return 0

# revealed: def guarded(x: some <Protocol with members 'foo'>) -> Literal[0]
reveal_type(guarded)
```

### a place that says what it holds says it about the parameter too

reading a *member* into a declared place constrains that member. reading the parameter itself into
one constrains the parameter, for the same reason and by the same rule

```py
def stored(x):
    a: int = x
    return a

reveal_type(stored)  # revealed: def stored(x: some int) -> x

stored("no")  # error: [invalid-argument-type]
```

```py
def returned(x) -> str:
    return x

reveal_type(returned)  # revealed: def returned(x: some str) -> str

returned(1)  # error: [invalid-argument-type]
```

### an operation on a hole nothing bounded answers gradually

a hole nothing bounded is the gradual type it replaced, so it proves no more than that type did.
resolving the operator against it would let the *other* operand decide the result — `int * x` would
read as `int`, which `scale(3, 1.5)` disproves

```py
def scale(n: int, x):
    reveal_type(n * x)  # revealed: Unknown
    return n * x

reveal_type(scale(3, 1.5))  # revealed: Unknown
```

a hole the body *did* bound is a type like any other, and the operator resolves against it

```py
def twice(n=1):
    reveal_type(n * 2)  # revealed: int
```

### a name that was reassigned is no longer the parameter

uses are recognised by type, not by spelling

```py
class A:
    def foo(self) -> int:
        return 1

def f(x):
    x = A()
    x.foo()

f("anything")  # ok
```

### a parameter its own body rebinds says nothing

a name bound more than once cannot stand for one value, which is already why a reassigned local
contributes nothing. a parameter is no different once its own body rebinds it: below the rebinding
the name is whatever the rebinding produced, so what is done with it there requires nothing of what
the caller passed

keeping the requirements collected *above* the rebinding would not do either. walking a traceback
requires only that the argument have a `tb_next`, so the rebinding lands on that member's type —
which is `object`, because nothing said what it holds — and the read below it then fails against the
bound the function itself produced

```py
def deepest(tb):
    if tb.tb_next:
        tb = tb.tb_next
    return tb.tb_frame

deepest("anything")  # ok
```

the same holds when the member is a method, which is the shape most of these take

```py
def rebound(x):
    x.foo()
    x = x.foo()
    x.foo()

rebound("anything")  # ok
```

the rebinding does not have to be reachable, or to come from the parameter, for the name to stop
standing for one value

```py
def maybe(x, flag):
    x.foo()
    if flag:
        x = 1

maybe("anything", True)  # ok
```

### a recursive call does not constrain

```py
def f(x):
    if x:
        return f(x)
    return 1

f("anything")  # ok
```

### a default and a use both have to hold

```py
class Str(str):
    def extra(self) -> int:
        return 1

def f(x="s"):
    x.extra()

f(Str())  # ok
f("plain")  # error: [invalid-argument-type]
f(1)  # error: [invalid-argument-type]
```

### requirements that cannot all hold fall back to gradual

a bound of `Never` would report the contradiction at every call site and never where it lives

```py
def takes_int(a: int) -> None: ...
def takes_str(a: str) -> None: ...
def f(x):
    takes_int(x)
    takes_str(x)

f(1)  # ok
f(object())  # ok
```

### a nested function's uses are its own

```py
class A:
    def foo(self) -> int:
        return 1

def outer(x):
    def inner(y):
        y.foo()

    return inner

outer("anything")  # ok
```

## a whole signature, recovered

```toml
[environment]
python-version = "3.13"

[analysis]
sound-types = true
```

the two halves compose. naming the parameter is what lets the return type refer back to it, so an
unannotated body gets the signature it would have been given by hand:

```by
def f(x="asdf"):
    return x.startswith("foo")

reveal_type(f)  # revealed: def f(x: some str = "asdf") -> x.startswith("foo")
reveal_type(f("foobar"))  # revealed: True
reveal_type(f("bar"))  # revealed: False
reveal_type(f())  # revealed: False
```

which is the signature `some` spells by hand:

```by
def written(s: some str) -> s.startswith("foo"):
    return s.startswith("foo")

reveal_type(written)  # revealed: def written(s: some str) -> s.startswith("foo")
```

a protocol member with no body returns `None`, because that is what running it would do:

```by
protocol X:
    def f(self)

def g(x: X):
    reveal_type(x.f())  # revealed: None
```

## a parameter a nested scope captures stays gradual

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = true
```

a nested body is checked against the bound this walk builds, but its expressions belong to another
scope's inference and so are invisible here. a bound built from the outer uses alone would then be
checked against inner uses it never saw, and report what the gradual type never did:

```py
class Conv:
    def register(self) -> None: ...
    def structure(self) -> int:
        return 1

def outer(converter):
    converter.register()

    def inner():
        # only the outer `register()` is visible to the analysis, so a bound built from it
        # would reject this
        return converter.structure()

    return inner

outer(Conv())  # ok
outer("anything")  # ok — captured, so nothing was required of it
```

a nested scope that captures nothing leaves the enclosing parameter alone:

```py
def unaffected(x):
    x.register()

    def inner(y):
        return y

    return inner

unaffected(Conv())  # ok
unaffected(1)  # error: [invalid-argument-type]
```

## a shape this analysis invented is not a requirement

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = true
```

reading a member off a hole leaves the shape this analysis invented for that member, and no type
variable to recognise it by. passing it back in says nothing a caller could fail — the requirement
would be on the very type being written — so the position it lands in stays gradual:

```py
class Inner:
    def b(self, other: int) -> None: ...

class Outer:
    a: Inner

def f(x):
    x.a.b(x.a)

f(Outer())  # ok
f(1)  # error: [invalid-argument-type]
```

a value that reaches the argument through a call carries that shape just as much:

```py
def identity(t):
    return t

def g(x):
    x.a.b(identity(x.a))

g(Outer())  # ok
g(1)  # error: [invalid-argument-type]
```

## a shape the program states is a requirement

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = true
```

only the shapes this analysis invents for itself are ruled out. a structural protocol the *program*
supplies is a requirement a call site can fail, so it bounds a hole like any other type — however it
is spelled, and whether it was written down or established by a narrowing

### written as a class

```by
from typing import Protocol

class P(Protocol):
    a: int

def takes(v: P) -> None: ...

def f(y):
    takes(y)

reveal_type(f)  # revealed: def f(y: some P)
```

### written inline

the same shape in basedpython's own notation says exactly as much

```by
def takes(v: protocol(a: int)) -> None: ...

def f(y):
    takes(y)

reveal_type(f)  # revealed: def f(y: some protocol(a: int))
```

### established by `hasattr`

```by
def f(x, y: object):
    if hasattr(y, "a"):
        x.b(y)

# revealed: def f(x: some protocol(def b(self, protocol(a: object), /) -> Unknown), y: object)
reveal_type(f)
```

### established by a sequence pattern

```by
def f(x, y: object):
    match y:
        case [_]:
            x.b(y)

# revealed: def f(x: some protocol(def b(self, Sequence[object] & protocol(def __getitem__(self, index: 0, /) -> object; def __len__(self, /) -> 1 | True) & not str & not bytes & not bytearray, /) -> Unknown), y: object)
reveal_type(f)
```

### established by a key-membership test

```by
from typing import TypedDict

class T(TypedDict):
    k: int

def f(x, y: T | dict[str, int]):
    if "k" in y:
        x.b(y)

# revealed: def f(x: some protocol(def b(self, T | (dict[str, int] & protocol(def __contains__(self, key: "k", /) -> True)), /) -> Unknown), y: T | dict[str, int])
reveal_type(f)
```

## an `async` override is wrapped once

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = true
```

what an override inherits is the base's *raw* return type. the coroutine wrapping an `async def`
gets is applied to the signature once, so copying the already-wrapped form would wrap a coroutine in
a coroutine and no override could ever satisfy its base:

```py
from abc import ABC, abstractmethod

class Base(ABC):
    @abstractmethod
    async def f(self): ...
    @abstractmethod
    async def g(self) -> None: ...

class Sub(Base):
    async def f(self):
        pass

    async def g(self):
        pass

reveal_type(Base.f)  # revealed: def f(self) -> CoroutineType[Any, Any, None]
reveal_type(Sub.f)  # revealed: def f(self) -> CoroutineType[Any, Any, None]
reveal_type(Sub.g)  # revealed: def g(self) -> CoroutineType[Any, Any, None]
```

## a bound reads back as the protocol that declares it

```toml
[environment]
python-version = "3.13"

[analysis]
sound-types = true
```

a synthesized bound is spelled as the [inline protocol](basedpython_inline_protocol.md) that would
declare it, so the whole recovered signature is something you could have written by hand:

```by
def f(x):
    a: int = x.foo()
    return x

reveal_type(f)  # revealed: def f(x: some protocol(def foo(self, /) -> int)) -> x

protocol Ok:
    def foo(self) -> int

protocol Bad:
    def foo(self) -> str

def m(ok: Ok, bad: Bad):
    reveal_type(f(ok))  # revealed: Ok
    # error: [invalid-argument-type] "Argument type `Bad` does not satisfy `protocol(def foo(self, /) -> int)`, inferred for parameter `x`"
    f(bad)
```

## a receiver is not a hole

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = true
```

`__new__` is an implicit staticmethod, so it has no `self` to be given the implicit annotation — but
construction still binds the class to its first parameter. Nothing a call site writes lands there,
so it is not a hole for one to fill:

```py
from typing import Protocol, Self
from ty_extensions import static_assert
from ty_extensions._internal import is_assignable_to

class Callback[T](Protocol):
    def __call__(self, *args, **kwargs) -> T: ...

class HasNew:
    def __new__(cls, x: int = 0, /) -> Self:
        return super().__new__(cls)

# the class object is callable and hands back an instance, whether or not a type variable stands
# in for it
static_assert(is_assignable_to(type[HasNew], Callback[HasNew]))

def _[T: HasNew](_: T):
    static_assert(is_assignable_to(type[T], Callback[HasNew]))
```

## lambda parameter defaults

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

a lambda parameter with a default follows the same rule as a function parameter, and the lambda's
own signature is checked at its call sites:

```py
g = lambda a=1: a
reveal_type(g)  # revealed: (a: int = 1) -> int

g(2)  # ok
g("x")  # error: [invalid-argument-type]
```

a `Callable` type context still takes priority over the default:

```py
from typing import Callable

cb: Callable[[str], str] = lambda a="s": a
reveal_type(cb)  # revealed: (a: str = "s") -> str
```

## its own option

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = true
```

recovering an unannotated signature is its own option. `sound-types` is a mode for inferring a
precise type wherever one is available, so it implies this; the dedicated option turns it on by
itself, without the rest of `sound-types`:

```py
class A:
    def foo(self) -> int:
        return 1

def f(x):
    x.foo()
    return 1

reveal_type(f)  # revealed: def f(x: some <Protocol with members 'foo'>) -> Literal[1]

f(A())  # ok
f(1)  # error: [invalid-argument-type]
```

## its own option, turned off

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = false
```

```py
def f(x):
    return 1

reveal_type(f)  # revealed: def f(x) -> Unknown

f("anything")  # ok
```

## the standard library does not ask

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = false
```

a vendored stub is part of the language ty defines, not the consumer's own code, so what its
unannotated forms mean cannot depend on a consumer's setting. `list.append` leaves its return type
out and still returns `None` with the option off:

```py
xs = [1]
reveal_type(xs.append(2))  # revealed: None
reveal_type(xs.sort())  # revealed: None
reveal_type(print("x"))  # revealed: None
```

## a first-party stub follows the option

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = false
```

a `.pyi` in the user's own tree is their code, and its unannotated forms answer the way the rest of
their code does:

`lib.pyi`:

```pyi
def f(x: int): ...
```

```py
from lib import f

reveal_type(f(1))  # revealed: Unknown
```

## a first-party stub follows the option, turned on

```toml
[environment]
python-version = "3.13"

[analysis]
infer-unannotated-signatures = true
```

`lib.pyi`:

```pyi
def f(x: int): ...
```

```py
from lib import f

reveal_type(f(1))  # revealed: None
```

## unannotated overrides

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

an unannotated method inherits the parameter and return types of the method it overrides:

```py
class Base:
    def m(self, a: int, b: str = "x") -> bytes:
        return b""

class Sub(Base):
    def m(self, a, b="y"):
        reveal_type(a)  # revealed: int
        reveal_type(b)  # revealed: str
        return b""

reveal_type(Sub().m)  # revealed: bound method Sub.m(a: int, b: str = "y") -> bytes

Sub().m(1)  # ok
Sub().m("nope")  # error: [invalid-argument-type]
```

the lookup starts *after* the class itself, so it finds the overridden method rather than the method
being defined. a method that overrides nothing stays gradual:

```py
class Standalone:
    def m(self, a):
        reveal_type(a)  # revealed: a@m
```

an explicit annotation on the override always wins:

```py
class Explicit(Base):
    def m(self, a: str, b: str = "y") -> bytes:  # error: [invalid-method-override]
        reveal_type(a)  # revealed: str
        return b""
```

## protocol and abstract members

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

`Protocol` members and `abstractmethod` declarations are ordinary base methods for this purpose:

```py
from typing import Protocol
from abc import ABC, abstractmethod

class P(Protocol):
    def run(self, a: int) -> str: ...

class Impl(P):
    def run(self, a):
        reveal_type(a)  # revealed: int
        return ""

class A(ABC):
    @abstractmethod
    def go(self, x: bytes) -> None: ...

class B(A):
    def go(self, x):
        reveal_type(x)  # revealed: bytes
```

## inferred return types

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

a function with no return annotation returns what its body returns.

### the headline

```py
def f():
    return 1

reveal_type(f())  # revealed: Literal[1]
```

### a body that can fall off the end also returns `None`

```py
def f(flag: bool):
    if flag:
        return 1

reveal_type(f(True))  # revealed: Literal[1] | None
```

### a body with no `return` at all returns `None`

```py
def f():
    print("hi")

reveal_type(f())  # revealed: None
```

### an empty body returns `None`

there is nothing to return, and running it gives `None`:

```py
from typing import Protocol
from abc import ABC, abstractmethod

class P(Protocol):
    def f(self): ...

class A(ABC):
    @abstractmethod
    def g(self): ...

def h(p: P, a: A):
    reveal_type(p.f())  # revealed: None
    reveal_type(a.g())  # revealed: None
```

`pass` says the same thing:

```py
def empty():
    pass

reveal_type(empty())  # revealed: None
```

### several returns union

```py
def f(flag: bool):
    if flag:
        return 1
    else:
        return "a"

reveal_type(f(True))  # revealed: Literal[1, "a"]
```

### a bare `return` is `None`

```py
def f(flag: bool):
    if flag:
        return
    return 1

reveal_type(f(True))  # revealed: None | Literal[1]
```

### a body that always raises returns `Never`

```py
def f():
    raise NotImplementedError

reveal_type(f())  # revealed: Never
```

### a nested function's returns are its own

```py
def outer():
    def inner():
        return 1

    return "outer"

reveal_type(outer())  # revealed: Literal["outer"]
```

### the inferred type is checked at use sites

```py
def f():
    return 1

f().bit_length()  # ok
f().upper()  # error: [unresolved-attribute]
```

### an explicit annotation always wins

```py
def f() -> object:
    return 1

reveal_type(f())  # revealed: object
```

### recursion

a function that calls itself is inferred from the returns that do not recurse:

```py
def fact(n: int):
    if n < 2:
        return 1
    return n * fact(n - 1)

reveal_type(fact(5))  # revealed: int
```

### recursion with no fixed point

a body that wraps its own result grows a constructor deeper every time it is read, so there is no
type it settles on. the divergent part is marked rather than chased:

```py
def recur(a):
    return [recur(b) for b in a]

reveal_type(recur([]))  # revealed: list[Divergent]
```

### recursion no marker survives

that marker only survives while the type is *built*. a body that hands itself to a generic call gets
its return type back out of that call's solve, which leaves a concrete type behind with nothing to
fold on — `map(g, n)` is `map[map[...]]` receding by one constructor every round. the recursion is
recognised by a round adding nothing but depth, and collapsed onto the marker one level in:

```py
def g(n):
    return map(g, n)

reveal_type(g([]))  # revealed: map[Divergent]
```

the same holds when the recursion goes round two functions:

```py
def to(n):
    return map(fro, n)

def fro(n):
    return map(to, n)

# the mutual case settles a level shallower than the single-function one above, which is the
# recursion being recognised on a different round rather than anything the program says. here it
# is shallower still: the marker is the whole answer, because the depth bound is what recognised
# the recursion and the bound replaces the value rather than folding into it
reveal_type(to([]))  # revealed: Divergent
```

### recursion that grows a tuple

a body that concatenates onto its own result adds an element every round, so no tuple length is the
answer — the length is the analysis counting its own rounds. it is given up, and the element type,
which the body really does determine, is kept:

```py
def shape(tensor):
    if not hasattr(tensor, "__iter__"):
        return ()
    return (len(tensor),) + shape(tensor[0])

reveal_type(shape([]))  # revealed: tuple[int, ...]
```

the same holds when the recursion goes round two functions:

```py
def outer(a):
    if a:
        return ()
    return inner(a)

def inner(b):
    return (1,) + outer(b)

reveal_type(outer([]))  # revealed: tuple[Literal[1], ...]
```

### a tuple whose elements are the round that built them

an attribute taken apart and put back together is defined in terms of itself, so its elements are
the marker. giving up the length would union those elements into one — and an element standing for
the cycle is precisely what the marker replaces, so the widened tuple has to be handed back through
the marker rather than around it. a round that widened and a round that marked would otherwise each
undo the other, and the two answers would alternate without either ever being reached:

```py
class C:
    def __init__(self):
        self._t = (0, 0)

    def f(self):
        a, b = self._t
        reveal_type(a)  # revealed: Divergent
        self._t = (a, b)
        reveal_type(self._t)  # revealed: tuple[Divergent, Divergent]
```

### generators

```py
def gen():
    yield 1
    yield "a"

reveal_type(gen())  # revealed: GeneratorType[Literal[1, "a"], Unknown, None]

def with_return():
    yield 1
    return "done"

reveal_type(with_return())  # revealed: GeneratorType[Literal[1], Unknown, Literal["done"]]

def delegating():
    yield from [1, 2]

reveal_type(delegating())  # revealed: GeneratorType[int, Unknown, None]
```

### async

```py
async def f():
    return 1

reveal_type(f())  # revealed: CoroutineType[Any, Any, Literal[1]]

async def agen():
    yield 1

reveal_type(agen())  # revealed: AsyncGeneratorType[Literal[1], Unknown]
```

### disabled

```toml
[environment]
python-version = "3.12"

[analysis]
infer-unannotated-signatures = false
```

```py
def f():
    return 1

reveal_type(f())  # revealed: Unknown
```

## bare `ClassVar`

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true
```

a bare `ClassVar` uses the inferred type, the same way an unannotated class-body assignment already
does. without this, adding `ClassVar` — a strengthening of intent — would *degrade* the type:

```py
from typing import ClassVar

class C:
    x: ClassVar = 1
    y = 1

reveal_type(C.x)  # revealed: int
reveal_type(C.y)  # revealed: int
```

## empty collection literals

```toml
[environment]
python-version = "3.13"

[analysis]
sound-types = true
```

an empty collection literal has element type `Never`, so passing one straight to a generic call
solves from it precisely instead of leaking `Unknown`. a non-empty literal is unaffected:

```py
def first[T](xs: list[T]) -> T:
    return xs[0]

reveal_type(first([]))  # revealed: Never
reveal_type(first([1]))  # revealed: int
```

## disabled

```toml
[environment]
python-version = "3.12"

[analysis]
infer-unannotated-signatures = false
```

with the option off, the gradual guarantee holds throughout.

```py
from typing import ClassVar

def f(a=1):
    reveal_type(a)  # revealed: Unknown | Literal[1]

f("x")  # ok

g = lambda a=1: a
reveal_type(g)  # revealed: (a=1) -> Unknown | Literal[1]

class Base:
    def m(self, a: int) -> bytes:
        return b""

class Sub(Base):
    def m(self, a):
        reveal_type(a)  # revealed: Unknown
        return b""

class C:
    x: ClassVar = 1

reveal_type(C.x)  # revealed: Unknown | Literal[1]

def first[T](xs: list[T]) -> T:
    return xs[0]

reveal_type(first([]))  # revealed: Unknown
```

## a redundant `-> None`

```toml
[environment]
python-version = "3.12"

[analysis]
infer-unannotated-signatures = true

[rules]
redundant-return-annotation = "warn"
```

an explicit `-> None` is reported when deleting it would leave the same type. where that type comes
from does not matter — only whether the two words change anything.

### the headline

```py
def f() -> None:  # error: [redundant-return-annotation]
    print("hi")

def g():  # ok — says the same thing
    print("hi")

reveal_type(f)  # revealed: def f()
reveal_type(g)  # revealed: def g()
```

### a bare `return` and `return None`

```py
def f() -> None:  # error: [redundant-return-annotation]
    return

def g() -> None:  # error: [redundant-return-annotation]
    return None
```

### `async def`

```py
async def f() -> None:  # error: [redundant-return-annotation]
    print("hi")
```

### a nested function

```py
def outer():
    def inner() -> None:  # error: [redundant-return-annotation]
        print("hi")
```

### a body with nothing in it

there is nothing to return, so `None` is what running it gives — the same type the annotation
states:

```py
from abc import ABC, abstractmethod
from typing import Protocol

class P(Protocol):
    def f(self) -> None: ...  # error: [redundant-return-annotation]

class A(ABC):
    @abstractmethod
    def g(self) -> None: ...  # error: [redundant-return-annotation]

def h() -> None: ...  # error: [redundant-return-annotation]
def i() -> None:  # error: [redundant-return-annotation]
    """just a docstring"""
```

### another annotation is not reported

```py
def f() -> int:
    return 1

def g() -> "None":
    print("hi")
```

only a literal `None` is reported; a quoted one is left alone.

### an `init(...)`

the parser gives the shorthand its `-> None`, so there is nothing in the source to remove:

```by
class A:
    init(let value: int)

class B:
    init(self, value: int):
        self.value = value
```

### a body that always raises

`Never` is what the body hands back, so `None` is a real widening:

```py
def f() -> None:
    raise ValueError
```

### a generator

a generator hands back a generator, so deleting the annotation would change the type. it already
draws `invalid-return-type`, and calling it redundant on top would advise a change that alters the
meaning:

```py
def f() -> None:  # error: [invalid-return-type]
    yield 1
```

### an overload group whose declarations return `None`

deleting the implementation's annotation makes it inherit the union of the declarations, which is
`None`; each declaration recovers `None` from its own empty body:

```py
from typing import overload

@overload
def f(a: int) -> None: ...  # error: [redundant-return-annotation]
@overload
def f(a: str) -> None: ...  # error: [redundant-return-annotation]
def f(a: int | str) -> None:  # error: [redundant-return-annotation]
    print(a)
```

### an overload group whose declarations do not

the implementation would inherit `int | str`, so its `-> None` is load-bearing and is not reported —
even though writing it is an overload error on its own:

```py
from typing import overload

@overload
def f(a: int) -> int: ...  # error: [invalid-overload]
@overload
def f(a: str) -> str: ...  # error: [invalid-overload]
def f(a: int | str) -> None:
    return None
```

### an override

inheriting a whole signature is gated on `sound-types`, but a *return type* the base already
declares is recovered whenever signatures are, so deleting `-> None` here would leave `m` returning
what `Base.m` returns:

```py
class Base:
    def m(self) -> int | None:
        return 1

class Sub(Base):
    def m(self) -> None:  # ok — without it, `m` would return `int | None`
        print("hi")
```

## a redundant `-> None`, in a stub

```toml
[environment]
python-version = "3.12"

[analysis]
infer-unannotated-signatures = true

[rules]
redundant-return-annotation = "warn"
```

the option is resolved per module, so a stub that has it on recovers `None` from a bodyless `def`
just like anything else:

`mod.pyi`:

```pyi
def f() -> None: ...  # error: [redundant-return-annotation]
def g() -> int: ...
```

`main.py`:

```py
from mod import f

reveal_type(f())  # revealed: None
```

## a redundant `-> None`, with the option off

```toml
[environment]
python-version = "3.12"

[analysis]
infer-unannotated-signatures = false

[rules]
redundant-return-annotation = "warn"
```

with nothing recovering the signature, `-> None` is load-bearing — dropping it widens the return
type to `Unknown`:

```py
def f() -> None:
    print("hi")

def g():
    print("hi")

reveal_type(f())  # revealed: None
reveal_type(g())  # revealed: Unknown
```

## a redundant `-> None`, under `sound-types`

```toml
[environment]
python-version = "3.12"

[analysis]
sound-types = true

[rules]
redundant-return-annotation = "warn"
```

`sound-types` implies recovering the signature, so it turns this on too.

### the headline, under `sound-types`

```py
def f() -> None:  # error: [redundant-return-annotation]
    print("hi")
```

### an override under `sound-types`

here an unannotated override does inherit the base's return type, so whether `-> None` is redundant
is decided by the base rather than by the body:

```py
class Base:
    def same(self) -> None:  # error: [redundant-return-annotation]
        print("hi")

    def different(self) -> int | None:
        return 1

class Sub(Base):
    def same(self) -> None:  # error: [redundant-return-annotation]
        print("hi")

    def different(self) -> None:
        print("hi")
```

## a redundant `-> None`, off by default

```toml
[environment]
python-version = "3.12"

[analysis]
infer-unannotated-signatures = true
```

the mdtest harness turns this rule off, so nothing is reported without the `[rules]` block above:

```py
def f() -> None:
    print("hi")
```
