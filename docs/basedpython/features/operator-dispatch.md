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

## a receiver that may be one of several

each possible class of the receiver is asked on its own. when `x` is an `int | float`, `x + 1.0` is a
`float`: an `int` rejects the `float` and `float.__radd__` answers
