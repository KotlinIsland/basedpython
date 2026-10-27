# sealed classes

a `sealed` class declares a closed hierarchy: it may be subclassed freely from
anywhere within its own workspace, but not from a dependency. the type checker
knows the full set of subclasses, and every sealed class exposes a
`__sealed_members__` tuple of those subclasses at runtime

`sealed` is a class modifier, written ahead of `class` like `final` or `abstract`:

```by
sealed class Shape
class Circle(Shape)
class Square(Shape)
```

transpiles to:

```python
class Shape: ...
Shape.__sealed_members__ = ()
class Circle(Shape): ...
Shape.__sealed_members__ += (Circle,)
class Square(Shape): ...
Shape.__sealed_members__ += (Square,)
```

## `__sealed_members__`

each sealed class gains a `__sealed_members__` attribute: a tuple of the direct
subclasses declared in the same scope as it. the tuple starts empty, right after
the sealed class's own statement, and each subclass is added right after its own
class statement, in the block it is written in, as above. so the tuple holds the
subclasses whose statements have run, in the order they ran: one written under an
`if` that is not taken, or in a loop, is in it once its statement has run

```by
sealed class Shape
class Circle(Shape)
class Square(Shape)

reveal_type(Shape.__sealed_members__)  # (*: type[Circle | Square])
```

the checker does not know the tuple's length, since which subclass statements have
run is not known statically, so it types it as any number of the subclasses

only direct subclasses declared in the same scope are members — a subclass of a
subclass, a subclass declared inside a function or class body nested in that
scope, or a subclass defined in another module, is not listed. this matches what
the runtime transform can name: it only ever sees one file, and builds the tuple
in the scope that declares the sealed class

a sealed class declared in a function or class body gets its tuple there, from
the subclasses declared alongside it:

```by
def make():
    sealed class Shape
    class Circle(Shape)
    class Square(Shape)
    return Shape

class Shapes:
    sealed class Shape
    class Circle(Shape)
```

transpiles to:

```python
def make():
    class Shape: ...
    Shape.__sealed_members__ = ()
    class Circle(Shape): ...
    Shape.__sealed_members__ += (Circle,)
    class Square(Shape): ...
    Shape.__sealed_members__ += (Square,)
    return Shape

class Shapes:
    class Shape: ...
    Shape.__sealed_members__ = ()
    class Circle(Shape): ...
    Shape.__sealed_members__ += (Circle,)
```

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
`__sealed_members__` assignments are inserted. there is no reverse transform, so
round-tripping python back to basedpython does not recover the `sealed` keyword
