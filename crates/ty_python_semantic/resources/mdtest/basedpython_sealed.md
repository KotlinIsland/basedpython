# basedpython: `sealed` classes

A `sealed` class declares a closed hierarchy: it may be subclassed freely from anywhere within its
own workspace, but not from a dependency. Every sealed class exposes a `__sealed_members__` tuple of
the direct subclasses declared in the scope that declares it. The transpiler adds each subclass to
the tuple as its class statement runs, so the tuple holds the subclasses whose statements have run,
in the order they ran.

## `__sealed_members__` lists the same-module subclasses

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

## subclasses declared in another scope are not members

The tuple is added to in the scope that declares the sealed class, which cannot name a class
declared inside a nested function or class body.

```by
sealed class A
class B(A)

def make():
    class C(A): ...
    return C

class Holder:
    class D(A): ...

reveal_type(A.__sealed_members__)  # revealed: (*: type[B])
make()
assert A.__sealed_members__ == (B,)
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

`shapes.by`:

```by
sealed class Shape
```

`circle.by`:

```by
from shapes import Shape

class Circle(Shape): ...

reveal_type(Circle())  # revealed: final Circle
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
