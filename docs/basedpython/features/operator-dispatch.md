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
can't see the `int`, so it runs with every operand `T`'s bound takes. a union operand is rejected
only when each of its types is outside the domain, since one that may be inside it reaches the
method

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
value](#rejecting-by-value), without writing `NotImplementedType` in the return type. only the
operands it can be reached with move on

```python
class Money:
    def __add__(self, other: "Money | int") -> "Money":
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

an operand annotated `object`, or not annotated at all, claims nothing about what it takes, and
nothing it returns is reported. a `NotImplemented` it returns still hands back whatever reaches it.
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
[checked](#a-py-dunder-tests-its-operand-itself) says it with a `return NotImplemented` reached for
an operand inside the domain

### in a `.by` file

`NotImplemented` is a value like any other in a `.by` file, so a function that returns it has to
declare it. in a `.py` file a `return NotImplemented` is still accepted whatever the function
declares

```by
class A:
    def __add__(self, other: int) -> int:
        return NotImplemented  # error: [invalid-return-type]
```

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
