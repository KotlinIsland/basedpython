# basedpython: `is` / `is not` narrowing

In basedpython the `is` and `is not` keyword pair is a *type test*: its right-hand side is a type
expression, and the test asks whether the value has that type. The `===` and `!==` operators keep
Python's identity comparison. Narrowing mirrors that split.

## `is` narrows to the type named

```by
def f(a: object):
    if a is int:
        reveal_type(a)  # revealed: int
```

## `is not` narrows to the negation of it

```by
def f(a: object):
    if a is not int:
        reveal_type(a)  # revealed: not int
```

## `!==` keeps Python identity semantics

```by
def f(a: object):
    if a !== None:
        reveal_type(a)  # revealed: not None
```

## `===` keeps Python identity semantics

```by
def f(a: int | None):
    if a === None:
        reveal_type(a)  # revealed: None
```

## A literal names the type holding exactly that value

`None`, `True`/`False` and a number are all type expressions, so a test against one narrows to the
literal type it names. The runtime check that comes out is the equality — or, for `None`, the
identity — that decides membership of that type.

```by
def f(a: int | None):
    if a is None:
        reveal_type(a)  # revealed: None
    if a is not None:
        reveal_type(a)  # revealed: int
```

```by
def f(a: bool | int):
    if a is True:
        reveal_type(a)  # revealed: True
    if a is False:
        reveal_type(a)  # revealed: False
```

```by
def f(a: int | str):
    if a is 1:
        reveal_type(a)  # revealed: 1
    if a is "x":
        reveal_type(a)  # revealed: "x"
```

## An enum member names the type holding exactly that member

An enum member is a singleton, so `Literal[Color.RED]` holds one object and the test for it is
identity — which is also what the runtime compares.

```by
import enum

class Color(enum.Enum):
    RED = 1
    GREEN = 2

def f(c: Color):
    if c is Color.RED:
        reveal_type(c)  # revealed: Color.RED
    if c is not Color.RED:
        reveal_type(c)  # revealed: Literal[Color.GREEN]
```

The same holds for based-enum members:

```by
enum class Genre:
    case A, B

def g(x: Genre):
    if x is Genre.A:
        reveal_type(x)  # revealed: Genre.A
    if x is not Genre.A:
        reveal_type(x)  # revealed: Literal[Genre.B]
```

## An undecidable test is `bool`

The identity folds Python applies to the same operator have no place here: the right-hand side names
a type rather than the class object the same source spells as a value, so `x is int` is not "an
instance compared to a class" and must not collapse to `Literal[False]`.

```by
def f(x: object):
    b = x is int
    reveal_type(b)  # revealed: bool
    assert x is int
    reveal_type(x)  # revealed: int
```

## A test the types settle is its answer

Where the value's type decides the question, the test *is* that answer — which is what lets a reader
see that the branch it guards is already decided.

```by
def f(x: int):
    reveal_type(x is int)  # revealed: True
    reveal_type(x is not int)  # revealed: False
```

## A test against a disjoint type is reported

A test whose value can never have the type named is a constant: `is` never holds and `is not` always
does. Either the guarded branch is dead or the wrong type was named.

```by
def f(x: None):
    # error: [non-overlapping-type-test] "`None` and `int` are non-overlapping types, so this test is always `False`"
    if x is int:
        ...
```

`is not` inverts the constant.

```by
def f(x: None):
    # error: [non-overlapping-type-test] "`None` and `int` are non-overlapping types, so this test is always `True`"
    if x is not int:
        ...
```

A narrowed literal is tested as the literal, not as its class.

```by
def f():
    c = 1
    # error: [non-overlapping-type-test] "`1` and `bool` are non-overlapping types, so this test is always `False`"
    if c is bool:
        ...
```

A constructor call produces a value whose runtime class is exactly the class it names, so it is
disjoint from every unrelated class even though the class itself is open to subclassing.

```by
class A: ...

def f():
    a = A()
    # error: [non-overlapping-type-test] "`final A` and `str` are non-overlapping types, so this test is always `False`"
    if a is str:
        ...
```

## A test that could hold is not reported

```by
class A: ...

class B(A): ...

def f(o: object, a: A, x: int | str, u):
    if o is int:
        ...
    if a is B:
        ...
    if x is str:
        ...
    # a gradual value overlaps everything
    if u is int:
        ...
```

## A union target holds when any arm does

```by
def f(x: None):
    # error: [non-overlapping-type-test] "`None` and `int | str` are non-overlapping types, so this test is always `False`"
    if x is int | str:
        ...

def g(x: int):
    if x is int | str:
        ...
```

## Identity comparisons are left alone

`===` and `!==` keep Python identity semantics, where an always-`False` comparison is already typed
`Literal[False]`.

```by
def f(x: None):
    b = x === 1
    reveal_type(b)  # revealed: False
```

## A chained type test is rejected

Python chains `a is int is str` into `a is int and int is str`, whose second half asks whether the
*class* `int` has the type `str`. That is never what the writer meant, so the chain is refused
rather than given a meaning.

```by
def f(a: object):
    # error: [invalid-syntax] "`is` type test cannot be chained with another comparison; split it into separate tests joined with `and`"
    b = a is int is str
```

A chain of identity comparisons is ordinary Python and stays legal.

```by
def f(a: object, b: object):
    c = a === b !== None
```
