# sealed classes

a `sealed` class declares a closed hierarchy: it may be subclassed freely from
anywhere within its own workspace, but not from a dependency. the type checker
knows the full set of subclasses, and every sealed class exposes a
`__sealed_members__` tuple of those subclasses at runtime.

## surface syntax

`sealed` is a class modifier, written ahead of `class` like `final` or `abstract`:

```by
sealed class Shape
class Circle(Shape)
class Square(Shape)
```

## `__sealed_members__`

each sealed class gains a `__sealed_members__` attribute: a tuple of its
same-module direct subclasses, in source order.

```by
sealed class Shape
class Circle(Shape)
class Square(Shape)

reveal_type(Shape.__sealed_members__)  # (type[Circle], type[Square])
```

the transpiler materializes the tuple at runtime by emitting an assignment after
the last subclass:

```python
class Shape: ...
class Circle(Shape): ...
class Square(Shape): ...
Shape.__sealed_members__ = (Circle, Square)
```

only direct, same-module subclasses are members — a subclass of a subclass, or a
subclass defined in another module, is not listed. this matches what the runtime
transform can see (it only ever sees one file).

## workspace boundary

a sealed class may be subclassed from anywhere in the workspace that defines it,
including a different module:

```by
# shapes.by
sealed class Shape

# circle.by
from shapes import Shape

class Circle(Shape): ...  # fine — same workspace
```

subclassing a sealed class that lives in a dependency is an error, since the
dependency owns the closed hierarchy:

```by
from some_dependency import Shape

class Circle(Shape): ...  # error: [subclass-of-sealed-class]
```

## transpilation

`sealed` is a forward-only transform: the modifier keyword is stripped and the
`__sealed_members__` assignment is inserted. there is no reverse transform, so
round-tripping python back to basedpython does not recover the `sealed` keyword.
