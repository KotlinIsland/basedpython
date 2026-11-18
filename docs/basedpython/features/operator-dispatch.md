# operator dispatch

an operator dunder can tell its operand apart only by its class, not by its type arguments. so an
operand of the wrong class is handed back and the other side gets its turn, but an operand of the
right class with the wrong type arguments is a type error, with no fallback

```python
class Foo:
    def __iadd__(self, other: int) -> int: ...
    def __add__(self, other: str) -> str: ...

foo = Foo()
foo += "a"  # `__iadd__` rejects a `str`, so this is `foo.__add__("a")`
reveal_type(foo)  # str

xs: list[int] = [1, 2, 3]
xs += ["a"]  # error: `list.__iadd__` runs with any `Iterable`, and this one holds a `str`
```

## the rule

python calls an operator dunder with whatever the other operand is. the method either rejects it,
by returning `NotImplemented`, and python moves on — to the other operand's reflected dunder, or from
`__iadd__` to `__add__` — or it runs with it

a method can only reject what a runtime check can see, and a runtime check sees the operand's class.
so each parameter is read two ways:

- **its domain** is the annotation with the type arguments taken off: `list[int]` is every `list`,
    `Iterable[T]` every `Iterable`. an operand outside the domain is rejected, and dispatch moves on
- **its full type** is what the method accepts. an operand inside the domain that the annotation
    doesn't accept reaches the method anyway, so the operation is reported as
    `unsupported-operator`

a literal, `None`, and a protocol are their own domains. a type variable's domain is that of its
bound, and that includes the class's own type parameters: in `Box[int]`, a `__add__(self, other: T)`
can't see the `int`, so it runs with every operand `T`'s bound takes. a union operand is split: its
types inside the domain reach the method, and the rest are rejected. a type that may share a
subclass with the domain is both, as [below](#an-operand-that-may-be-both)

```python
class V[T]:
    def __add__(self, other: V[T]) -> V[T]: ...
    def __radd__(self, other: object) -> str: ...

def f(a: V[int], b: V[str]):
    a + b  # error: `V[int].__add__` can't tell `b` from a `V[int]`, so `__radd__` is never reached
```

this applies to binary operators, augmented assignment, and comparisons. a comparison that runs with
its operand doesn't fall back to identity either: `==` between two values whose `__eq__` takes the
other but doesn't accept it is an error, not an `is` test

## an operand that may be both

a class that isn't in the domain may still share a subclass with it. python lets a class inherit
from both, so an operand of the one may be an instance of the other, and then it reaches the method.
the operation may be either answer

```by
class Money: ...

class Wallet:
    def __add__(self, other: Money) -> int: ...

class Coupon:
    def __radd__(self, other: object) -> str: ...

class MoneyCoupon(Coupon, Money): ...

def f(w: Wallet, c: Coupon):
    reveal_type(w + c)  # int | str: a `MoneyCoupon` reaches `Wallet.__add__`
```

an operand is outside the domain, and answered by the other side alone, when nothing can be both:

- its class is exact: `c: final Coupon`, or, in a `.by` file, a value made by `Coupon()`. the
    `final T` of a type variable isn't exact, since `T` may be any subclass of its bound
- its class is `@final`, or is a literal
- the two classes are each a `@disjoint_base` (PEP 800), so no class may inherit from both
- their instances are laid out differently, as `int`'s and `float`'s are

with nothing to fall back to, an operand that may be outside the domain is `unsupported-operator`,
since python raises a `TypeError` for it

in a `.py` file this applies to a dunder whose operands are checked
(`analysis.checked-operator-operands`, on under the `strict` preset). elsewhere a `.py` dunder is
read as ty reads it: an operand that isn't assignable to the domain is outside it

## a subclass's reflected dunder answers as its ancestor's does

python asks the right operand first when its class is a subclass of the left operand's and, for a
binary operator, answers the reflected dunder differently. code typed against the ancestor can't see
that coming, so the reflected dunder has to answer as the ancestor's own dunder does, or it is
`invalid-operator-override`

```by
class A:
    def __add__(self, other: object) -> int: ...

class B(A):
    def __radd__(self, other: object) -> str: ...  # error: [invalid-operator-override]

def f(a1: A, a2: A):
    reveal_type(a1 + a2)  # int, which a `B` in `a2` would make `B.__radd__`'s `str`
```

a reflected dunder that doesn't take an `A` at all never answers for one: a
`__radd__(self, other: int)` hands an `A` back, and python asks `A.__add__`. it is also checked
where a class inherits it: in `class Both(Mixin, A)`, `Mixin.__radd__` answers `A() + Both()` before
`A.__add__`

a comparison asks a subclass's reflected dunder first even when the subclass inherits it, so a class
whose `__lt__` and `__gt__` disagree is reported too, unless it is `@final`

in a `.py` file this applies where the file's operator dunders are checked
(`analysis.checked-operator-operands`)

## a subclass's in-place dunder answers as its ancestor's binary dunder does

`x += y` asks `x`'s `__iadd__`, and only without one `x.__add__`. so a class that adds an
`__iadd__` to an ancestor without one changes what `+=` does to a value typed as the ancestor. the
in-place dunder may take only operands the ancestor's `__add__` takes, and has to answer them as it
does, or it is `invalid-operator-override`

```by
class Foo:
    def __add__(self, x: str) -> str: ...

class Bar(Foo):
    def __iadd__(self, x: object) -> int: ...  # error: [invalid-operator-override]

def f(x: Foo):
    foo = x
    foo += "a"
    reveal_type(foo)  # str, which a `Bar` in `x` would make `Bar.__iadd__`'s `int`
```

it may take fewer operands: one it hands back reaches the class's own `__add__`, which answers as the
ancestor's does. so the two are compared as an override is compared with the method it overrides,
but only on the operands the `__iadd__` takes, and called as python calls it, with one operand passed
positionally. that lets it take fewer operands, and holds it to the type arguments of an operand it
can't tell apart

```by
class Vec:
    def __add__(self, other: Vec) -> Vec: ...

class MutableVec(Vec):
    def __iadd__(self, other: MutableVec) -> Self: ...  # fine: another `Vec` reaches `Vec.__add__`

class Items:
    def __add__(self, other: list[int]) -> int: ...

class StrItems(Items):
    def __iadd__(self, other: list[str]) -> int: ...  # error: [invalid-operator-override]
```

`StrItems.__iadd__` takes every `list`, so `x += [1]` with `x: Items` hands it a `list[int]`

`Self` in the ancestor's `__add__` is the ancestor for code typed against it, but whatever class the
value is for code typed with a type variable bound to it, or inside one of its methods, so the
`__iadd__` has to answer both ways: a `-> MutableVec` there would answer a subclass of `MutableVec`
with something else. an operand annotated `Self` is tested against the class, so it may be of the
class without being the `Self` of a subclass, and `other: Self` doesn't fit. an `__iadd__` that
isn't a plain method tests nothing, so it takes every operand

## an operator dunder override takes only what the overridden dunder takes

an operand an ancestor's dunder hands back is answered by what python asks next, and the operation is
typed that way. an override that takes the operand answers it instead, so an operator dunder may take
only what the dunder it overrides takes, or it is `invalid-operator-override`. the ordinary override
check allows a wider parameter, and this is the one place where that changes what an operation does

```by
@final
class S:
    def __radd__(self, other: object) -> str: ...

class A:
    def __add__(self, other: int) -> int: ...

class Wide(A):
    override def __add__(self, other: object) -> int: ...  # error: [invalid-operator-override]

def f(a: A, s: S):
    reveal_type(a + s)  # str, which a `Wide` in `a` would make `Wide.__add__`'s `int`
```

`__eq__` and `__ne__` keep [their own rule](#__eq__-and-__ne__-may-take-fewer-operands)

## what isn't checked

a dunder the left operand's type doesn't have at all may still be on its runtime class. `o + t` with
`o: object` is answered by `t`'s `__radd__`, but a subclass of `object` that defines `__add__`
answers it first, and `foo += y` with `foo` typed as a protocol that has `__add__` and no `__iadd__`
is answered by whatever `__iadd__` the runtime class has. no class definition can see every such
operation, so this isn't reported. an operand of an exact class (`final T`, or a value made by a
constructor in a `.by` file) has no subclass to worry about

the stdlib's classes are taken at their word: `MutableSet.__ior__` answers what `Set.__or__` would
differently, and a class that inherits it isn't reported for it

a subclass's in-place dunder is compared with its ancestor's binary dunder as the override check
compares an override with the method it overrides, so it has that check's gaps:

- an overload of the ancestor's dunder is fitted with any overload of the in-place dunder that
    matches it, though python calls the first that matches
- the type parameters of a generic ancestor dunder are solved rather than holding the in-place
    dunder to every one of them, so `__iadd__(self, other: object) -> int` passes against
    `__add__[S](self, other: S) -> S`, and against `__add__[S](self: S, other: object) -> S`
- a generic subclass doesn't see an ancestor's overloads that annotate `self`, such as
    `__add__(self: Foo[int], x: int) -> int`

a comparison a `dataclass(order=True)` writes is read as taking every operand, though it hands back
an operand of another class, so a dataclass with `order=True` that subclasses a class with its own
`__lt__` is reported

## why `list` is the motivating case

a checker that treats every mismatch as a rejection picks `list.__add__` for `xs += ["a"]`, and types
`xs` as a new `list[int | str]`. but `list.__iadd__` never rejects an iterable — it extends the list
in place — so any other name for the same list now holds a `str` its type says it can't

## an overload covers its domain

an overloaded dunder rejects only an operand outside the domain of every overload

```python
class C:
    @overload
    def __add__(self, other: int) -> int: ...
    @overload
    def __add__(self, other: list[int]) -> str: ...

def f(c: C, strings: list[str]):
    c + strings  # error: the `list[int]` overload runs with any `list`
```

## only a plain method hands an operand back

python moves on only from a method that returns `NotImplemented`. a coroutine function or a
generator function never does: calling one hands back the coroutine or the generator, whatever the
operand. and the test that hands an operand back is only at the top of a plain `def` of the dunder,
in a class body, with the operand as its second parameter, so in a `.by` file, and in a `.py` file
whose dunders are [checked](#a-py-dunder-tests-its-operand-itself), none of these hands an operand
back either:

- a decorated method, which python calls through whatever the decorator made
- a function bound to the dunder under another name, like `__add__ = add`
- a method that gathers its operand into `*args`, or is a `@staticmethod` or `@classmethod`
- a method of an [extension](extensions.md), which is called directly rather than dispatched to

an operation on one of these runs the method with whatever the operand is, so an operand it doesn't
accept is `unsupported-operator`. so is a call that fails for a reason other than its operand, like
a method that needs a second argument

## a `.by` dunder tests its operand

in a `.by` file you don't write the rejection: the dunder tests its operand against the parameter's
domain before the body runs, and hands back one outside it

```by
class A:
    def __add__(self, other: int) -> int:
        return other + 1
```

transpiles to:

```python
class A:
    def __add__(self, other: int) -> int:
        if not isinstance(other, int): return NotImplemented
        return other + 1
```

so the body sees what the annotation says, and `A() + B()` reaches `B.__radd__` as the checker says
it does. the test is the one [`other is int`](parametric-type-tests.md) makes: a union tests each
arm, a literal its value, `None` by identity, a callable with `callable()`, and a
`@runtime_checkable` protocol the presence of its members. an `override` that leaves its operand
unannotated takes the type of the method it overrides, and is tested against that. an operand annotated `object`, or not
annotated at all and overriding nothing, is tested for nothing, and the method runs with every
operand. a method a protocol declares is never the one python calls, and is tested for nothing too

a default value of the operand passes the test: python's operators always pass the operand, so only
a direct call that leaves it out gets the default, and the default is checked against the annotation

a domain no runtime test can tell an operand apart from — a protocol that isn't
`@runtime_checkable`, a class imported only under `if TYPE_CHECKING:` — is an
`erased-type-check` error on the annotation

```by
from typing import Protocol

class HasX(Protocol):
    x: int

class A:
    def __add__(self, other: HasX) -> int:  # error: [erased-type-check]
        return other.x
```

a natively compiled dunder hands an operand that isn't of the class its parameter holds to the
interpreted definition, which makes the test. one whose test asks for more than that class — a
union, a protocol — is left to the interpreted definition

## a `.py` dunder tests its operand itself

nothing lowers a `.py` file, so a `.py` dunder has to be written with the test a `.by` one is
given. the checker holds it to that: until the body tests its operand, the operand is the
annotation or anything outside its domain

```python
class Money:
    def __add__(self, other: "Money") -> "Money":
        reveal_type(other)  # object
        if not isinstance(other, Money):
            return NotImplemented
        reveal_type(other)  # Money
        return Money()
```

an operand inside the domain matched the whole annotation at the operation that dispatched to the
method, so testing its class gets the annotation back: a `list[int]` operand is a `list[int]` once
`isinstance(other, list)` holds

a body that returns a value for an operand it never tested into the domain is
`unguarded-operand`. the operation has already been typed as moving on from that operand, so the
value answers something the checker said was answered somewhere else. falling off the end of the
body returns `None`, so it counts too, and a body that reassigns its operand is reported once,
since what it tests is then not what python handed it

```python
class Bad:
    def __add__(self, other: int) -> int:
        return 1  # error: [unguarded-operand] `other` may be outside `int` where this returns
```

a `raise`, or an `assert` whose test can fail, reached with an operand that may be outside the
domain is reported the same way: python stops there, where the operation was typed as moving on to
the other operand. a `return` inside a `try` is checked where it is written, and an override whose
operand takes its type from the method it overrides is checked against that type

a `return NotImplemented` reached with an operand inside the domain [rejects it by
value](#rejecting-by-value), so the return type says so with `NotImplementedType`. the test's own
`return NotImplemented` needn't be declared: it hands back only operands outside the domain, which
dispatch already moves past. only the operands a `return NotImplemented` can be reached with move on

```python
from types import NotImplementedType
from typing import final

@final
class Money:
    def __add__(self, other: "Money | int") -> "Money | NotImplementedType":
        if isinstance(other, int):
            if other == 0:
                return self
            return NotImplemented
        if not isinstance(other, Money):
            return NotImplemented
        return Money()

Money() + Money()  # Money: only an `int` is handed back
Money() + 1  # error: [unsupported-operator]: `int.__radd__` doesn't take a `Money`
```

without `@final`, a `Money` operand [may also be an `int`](#an-operand-that-may-be-both), through a
class that inherits from both, so `Money() + Money()` may be handed back too, and is
`unsupported-operator`

an operand annotated `object`, or not annotated at all, claims nothing about what it takes, so it
has no test to write, and nothing it returns is `unguarded-operand`. a `NotImplemented` it returns
hands back whatever reaches it, so it has to be declared.
this is `analysis.checked-operator-operands`, which is on under the `strict` preset and off under
`ty-compatible`, where the body sees the annotation as if python only passed what it says

## rejecting by value

a dunder can also hand back an operand of the right class, after testing its value. it says so by
writing `NotImplementedType` in its return type, and the operation then also takes whatever python
asks next

```python
from types import NotImplementedType

class Small:
    def __add__(self, other: int) -> int | NotImplementedType:
        if other > 1:
            return NotImplemented
        return 0

Small() + 1  # error: [unsupported-operator]: `int.__radd__` doesn't take a `Small`
```

- after a binary dunder that may reject, python asks the other operand's reflected dunder — only
    when the operands are of different types. when nothing takes the operand, the operation is
    `unsupported-operator`
- after an in-place dunder that may reject, python asks the binary dunder, so `x += y` may also
    rebind `x` to what `__add__` returns
- after a comparison that may reject, `==` and `!=` fall back to identity

an operation never has `NotImplementedType` in its own type: a `NotImplemented` the dunder hands back
is the operation moving on

a return type inferred from the body says this only when it is nothing but `NotImplementedType`.
otherwise it may only count the `return NotImplemented` that rejects an operand of the wrong class,
which the domain already covers. a `.py` dunder whose body is
[checked](#a-py-dunder-tests-its-operand-itself) says it in its return type too, but its body says
which operands: those a `return NotImplemented` is reached with. so the body is read, and the return
type only where the body can't say, as where a `return` reached with an operand inside the domain
returns `Any`, or where the method only declares itself, in a protocol or as an `@abstractmethod`

### declaring it

`NotImplemented` is a value like any other in a `.by` file, so a function that returns it has to
declare it

```by
class A:
    def __add__(self, other: int) -> int:
        return NotImplemented  # error: [invalid-return-type]
```

so it is in a `.py` file whose operator dunders are
[checked](#a-py-dunder-tests-its-operand-itself), but for the `return NotImplemented` of a dunder's
test, which is reached only with operands outside the domain

```python
class A:
    def __add__(self, other: int) -> int:
        if not isinstance(other, int):
            return NotImplemented  # fine: the test
        return NotImplemented  # error: [invalid-return-type]: an `int` is handed back
```

elsewhere a `.py` `return NotImplemented` is accepted whatever the function declares, as python's
other checkers accept it

## `__eq__` and `__ne__` may take fewer operands

an `__eq__` or `__ne__` override can narrow its operand, so a class can say what it compares with

```by
class Point:
    x: int

    def __eq__(self, other: Point) -> bool:
        return self.x == other.x

Point() == 1  # `Point.__eq__` hands back the `int`, and the comparison ends at `is`: `bool`
```

an operand an `__eq__` hands back moves on to the other operand's `__eq__`, and then to an identity
test. so it still compares with every operand and still answers with a `bool`, the same as
`object.__eq__(self, other: object) -> bool`, which itself hands back every operand but its
receiver. writing `NotImplementedType` in the return type is a valid override for the same reason

what it answers with is still checked: an override that narrows has to answer with something a
`bool` is assignable to wherever the method it overrides does, or it is `invalid-method-override`.
no other operator gets this: `<` with nothing that takes the operand is a `TypeError`, so a `__lt__`
override that narrows its operand is still `invalid-method-override`

this holds only where the override really hands back what it doesn't take: a `.by` override
[tests its operand](#a-by-dunder-tests-its-operand) before its body runs, and a `.py` one is held to
writing the test [where the project checks it](#a-py-dunder-tests-its-operand-itself). an unchecked
`.py` `__eq__` that narrows its operand runs its body with whatever the other operand is, so it is
`invalid-method-override`, as it is in python's other checkers

## a receiver that may be one of several

each possible class of the receiver is asked on its own. when `x` is an `int | float`, `x + 1.0` is a
`float`: an `int` rejects the `float` and `float.__radd__` answers
