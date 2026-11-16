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
bound

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
`@runtime_checkable` protocol the presence of its members. an operand annotated `object`, or not
annotated at all, is tested for nothing, and the method runs with every operand

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

Small() + 1  # error: when `__add__` hands the `int` back, `int.__radd__` doesn't take a `Small`
```

- after a binary dunder that may reject, python asks the other operand's reflected dunder — only
    when the operands are of different types. when nothing takes the operand, the operation is
    `unsupported-operator`
- after an in-place dunder that may reject, python asks the binary dunder, so `x += y` may also
    rebind `x` to what `__add__` returns
- after a comparison that may reject, `==` and `!=` fall back to identity

an operation never has `NotImplementedType` in its own type: a `NotImplemented` the dunder hands back
is the operation moving on

only a return type you write says a dunder may reject by value. one inferred from the body also
counts the `return NotImplemented` that rejects an operand of the wrong class, which the domain
already covers

### in a `.by` file

`NotImplemented` is a value like any other in a `.by` file, so a function that returns it has to
declare it. in a `.py` file a `return NotImplemented` is still accepted whatever the function
declares

```by
class A:
    def __add__(self, other: int) -> int:
        return NotImplemented  # error: [invalid-return-type]
```

## a receiver that may be one of several

each possible class of the receiver is asked on its own. when `x` is an `int | float`, `x + 1.0` is a
`float`: an `int` rejects the `float` and `float.__radd__` answers
