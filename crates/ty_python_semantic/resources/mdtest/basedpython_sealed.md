# basedpython: `sealed` classes

A `sealed` class declares a closed hierarchy: it may be subclassed freely from anywhere within its
own workspace, but not from a dependency, and a value of it is an instance of one of those
subclasses. So a `match` covering them is exhaustive. Every sealed class exposes a
`__sealed_members__` tuple of them. The transpiler adds each subclass to the tuple as its class
statement runs, so the tuple holds the subclasses whose statements have run, in the order they ran.

## A `match` covering every subclass is exhaustive

```by
sealed class A
class B(A)
class C(A)

def statement_expression(a: A) -> str:
    let result = match a:
        case B(): "b"
        case C(): "c"
    return result  # no diagnostic

def falls_through(a: A) -> int:
    match a:
        case B():
            return 1
        case C():
            return 2
```

Leaving a subclass out is reported.

```by
def missing_case(a: A):
    # error: [non-exhaustive-statement-expression]
    let result = match a:
        case B(): "b"
```

## Narrowing excludes one subclass at a time

What is left of a sealed class after some of its subclasses are excluded is the others. The excluded
ones stay as negatives, since a class can derive from two siblings.

```by
sealed class A
class B(A)
class C(A)

def f(a: A):
    match a:
        case B():
            reveal_type(a)  # revealed: B
        case C():
            reveal_type(a)  # revealed: C & not B
        case _:
            reveal_type(a)  # revealed: Never

def g(a: A):
    if isinstance(a, B):
        reveal_type(a)  # revealed: B
    else:
        reveal_type(a)  # revealed: C & not B
```

## A negative unrelated to the hierarchy leaves the class as it is

It says nothing about which subclass a value is.

```by
sealed class A
class B(A)

def f(a: A):
    if not isinstance(a, int):
        reveal_type(a)  # revealed: A & not int
```

## A subclass of a subclass is covered by the subclass

```by
sealed class A
class B(A)
class C(A)
class D(B)

def f(a: A) -> int:
    match a:
        case B():
            return 1
        case C():
            return 2

def g(a: A):
    if not isinstance(a, D):
        reveal_type(a)  # revealed: (B & not D) | (C & not D)
```

## A sealed subclass splits in turn

```by
sealed class Top
sealed class Mid(Top)
class Left(Mid)
class Right(Mid)
class Other(Top)

def f(top: Top) -> int:
    match top:
        case Left() | Right():
            return 1
        case Other():
            return 2
```

## Naming a sealed subclass covers everything under it

```by
sealed class Top
sealed class Mid(Top)
class Left(Mid)
class Other(Top)

def f(top: Top) -> int:
    match top:
        case Mid():
            return 1
        case Other():
            return 2
```

## `self` is a value of the class

```by
sealed class A:
    def f(self) -> int:
        match self:
            case B():
                return 1
            case C():
                return 2

class B(A)
class C(A)
```

## A union with a sealed class

```by
sealed class A
class B(A)
class C(A)

def f(value: A | int) -> int:
    match value:
        case int():
            return 0
        case B():
            return 1
        case C():
            return 2
```

## A generic sealed class

A subclass's relation to its base's type parameters is whatever its class statement says, so a
subclass that cannot be a `Box[int]` needs no case.

```by
sealed class Box[in out T]
class Wrap[T](Box[T])
class OfInt(Box[int])
class OfStr(Box[str])
class OfList[T](Box[list[T]])

def f(box: Box[int]) -> int:
    match box:
        case Wrap():
            reveal_type(box)  # revealed: Wrap[int]
            return 1
        case OfInt():
            return 2
```

A subclass that can be one still does.

```by
def g(box: Box[str]) -> int:  # error: [invalid-return-type]
    match box:
        case Wrap():
            return 1
```

## A sealed class cannot be instantiated

A `match` over the subclasses covers every value only because no value is an instance of the sealed
class itself.

```by
sealed class A
class B(A)

A()  # error: [call-non-callable]
B()  # no diagnostic
```

## A `type[A]` can be called

It stands for a subclass as much as for the class itself.

```by
sealed class A

def f(cls: type[A]):
    cls()  # no diagnostic
```

## A class created through `type()` cannot join the hierarchy

The class statements of the workspace are what close the hierarchy, and a call makes no such
statement.

```by
sealed class A

X = type("X", (A,), {})  # error: [subclass-of-sealed-class]
```

## `__sealed_members__` lists the subclasses

A subclass may be declared under an `if`, in a loop or after a `return`, so how many of them the
tuple holds is not known before the program runs.

```by
sealed class A
class B(A)
class C(A)

reveal_type(A.__sealed_members__)  # revealed: (*: type[B | C])
assert A.__sealed_members__ == (B, C)
```

## a sealed class with no subclasses

```by
sealed class A

reveal_type(A.__sealed_members__)  # revealed: ()
assert A.__sealed_members__ == ()
```

## only direct subclasses are members

```by
sealed class A
class B(A)
class C(B)

reveal_type(A.__sealed_members__)  # revealed: (*: type[B])
assert A.__sealed_members__ == (B,)
```

## a sealed class declared in a function or class body

The members are the direct subclasses declared alongside it, in the same body.

```by
def make():
    sealed class A
    class B(A)
    class C(A)
    reveal_type(A.__sealed_members__)  # revealed: (*: type[B | C])
    return A

class Outer:
    sealed class X
    class Y(X)

reveal_type(Outer.X.__sealed_members__)  # revealed: (*: type[Y])
assert len(make().__sealed_members__) == 2
assert Outer.X.__sealed_members__ == (Outer.Y,)
```

## subclasses declared in another scope are members

A subclass adds itself to the tuple as its statement runs, wherever it is written, so one declared
in a function body is in the tuple once that function has run, and one a `match` has to cover.

```by
sealed class A
class B(A)

def make():
    class C(A): ...
    return C

class Holder:
    class D(A): ...

reveal_type(A.__sealed_members__)  # revealed: (*: type[B | C | D])
assert A.__sealed_members__ == (B, Holder.D)
make()
assert len(A.__sealed_members__) == 3

def f(a: A) -> int:  # error: [invalid-return-type]
    match a:
        case B():
            return 1
```

## a subclass declared conditionally

A subclass whose statement does not run is not in the tuple. One that has run is in it straight
away, so code after it in the same block reads it.

```by
import sys

flag = len(sys.argv) > 99

sealed class A
if flag:
    class B(A)
    assert A.__sealed_members__ == (B,)
class C(A)

reveal_type(A.__sealed_members__)  # revealed: (*: type[B | C])
assert A.__sealed_members__[-1] == C
assert len(A.__sealed_members__) == (2 if flag else 1)
```

A function that returns before a subclass's statement runs has only the subclasses declared before
the `return` in its tuple.

```by
def make(early: bool):
    sealed class Shape
    class Circle(Shape)
    if early:
        return Shape
    class Square(Shape)
    return Shape

assert len(make(True).__sealed_members__) == 1
assert len(make(False).__sealed_members__) == 2
```

## a subclass declared more than once

Each class statement adds the class it makes, so a name declared twice is in the tuple twice, once
for each class.

```by
sealed class A
class B(A)
first = B
class B(A)

assert A.__sealed_members__ == (first, B)
```

## a subclass naming the sealed class another way

A subclass is a member when its base is the sealed class, however the base is written.

```by
sealed class A
Base = A
class B(Base)

reveal_type(A.__sealed_members__)  # revealed: (*: type[B])
assert A.__sealed_members__ == (B,)
```

## a private sealed class

```by
private sealed class A
class B(A)

class Outer:
    private sealed class X
    class Y(X)
    members = X.__sealed_members__

reveal_type(A.__sealed_members__)  # revealed: (*: type[B])
assert A.__sealed_members__ == (B,)
assert Outer.members == (Outer.Y,)
```

## subclassing from within the workspace is allowed

A subclass in another module is as much a member as one next to the sealed class, so the `match` in
`shapes.by` has to cover `Circle`.

`shapes.by`:

```by
sealed class Shape
class Square(Shape)

reveal_type(Shape.__sealed_members__)  # revealed: (*: type[Circle | Square])

def not_covered(shape: Shape) -> int:  # error: [invalid-return-type]
    match shape:
        case Square():
            return 1
```

`circle.by`:

```by
from shapes import Shape, Square

class Circle(Shape): ...

reveal_type(Circle())  # revealed: final Circle

def covered(shape: Shape) -> int:
    match shape:
        case Square():
            return 1
        case Circle():
            return 2
```

## a subclass written in python is a member too

`shapes.by`:

```by
sealed class Shape
```

`circle.py`:

```py
from shapes import Shape

class Circle(Shape): ...
```

`main.by`:

```by
from shapes import Shape
from circle import Circle

def f(shape: Shape) -> int:
    match shape:
        case Circle():
            return 1
```

## a subclass in a package is a member

`shapes.by`:

```by
sealed class Shape
```

`pkg/__init__.by`:

```by
```

`pkg/nested/circle.by`:

```by
from shapes import Shape

class Circle(Shape): ...
```

`main.by`:

```by
from shapes import Shape

reveal_type(Shape.__sealed_members__)  # revealed: (*: type[Circle])
```

## subclassing a sealed class from a dependency is forbidden

A sealed class defined in a dependency belongs to that dependency's workspace, so it cannot be
extended from first-party code.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/dep.by`:

```by
sealed class Shape
```

`main.by`:

```by
from dep import Shape

class Circle(Shape): ...  # error: [subclass-of-sealed-class]
```

## a dependency's sealed class is closed over its own package

The subclasses a dependency declares are the whole hierarchy, whichever of its modules they are in.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/dep/__init__.by`:

```by
sealed class Shape
```

`/.venv/<path-to-site-packages>/dep/circle.by`:

```by
from dep import Shape

class Circle(Shape): ...
```

`main.by`:

```by
from dep import Shape
from dep.circle import Circle

def f(shape: Shape) -> int:
    match shape:
        case Circle():
            return 1
```

## A class statement in a function is one member

The statement runs per call, and each run is the same member, so the tuple does not grow:

```by
sealed class A

def make() -> type[A]:
    class C(A): ...
    return C

first = make()
second = make()
assert A.__sealed_members__ == (second,)
```
