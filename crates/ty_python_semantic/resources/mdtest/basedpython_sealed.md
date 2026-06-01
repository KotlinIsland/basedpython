# basedpython: `sealed` classes

A `sealed` class declares a closed hierarchy: it may be subclassed freely from anywhere within its
own workspace, but not from a dependency. Every sealed class exposes a `__sealed_members__` tuple of
its same-module direct subclasses, which the transpiler materializes at runtime.

## `__sealed_members__` lists the same-module subclasses

```by
sealed class A
class B(A)
class C(A)

reveal_type(A.__sealed_members__)  # revealed: (type[B], type[C])
```

## a sealed class with a single subclass

```by
sealed class A
class B(A)

reveal_type(A.__sealed_members__)  # revealed: (type[B],)
```

## a sealed class with no subclasses

```by
sealed class A

reveal_type(A.__sealed_members__)  # revealed: ()
```

## only direct subclasses are members

```by
sealed class A
class B(A)
class C(B)

reveal_type(A.__sealed_members__)  # revealed: (type[B],)
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

reveal_type(Circle())  # revealed: Circle
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
