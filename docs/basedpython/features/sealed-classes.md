# sealed classes

a `sealed` class declares a closed hierarchy: it may be subclassed freely from
anywhere within its own workspace, but not from a dependency, and every value of
it is an instance of one of those subclasses. so a `match` over the subclasses is
exhaustive:

```by
sealed class Shape
class Circle(Shape)
class Square(Shape)

def name(shape: Shape) -> str:
    return match shape:
        case Circle(): "circle"
        case Square(): "square"
```

`sealed` is a class modifier, written ahead of `class` like `final` or `abstract`.
the class declarations above transpile to:

```python
class Shape: ...
Shape.__sealed_members__ = ()
class Circle(Shape): ...
Shape.__sealed_members__ += (Circle,)
class Square(Shape): ...
Shape.__sealed_members__ += (Square,)
```

## exhaustiveness

the checker knows every class statement in the workspace with a sealed class as a
direct base. narrowing a value of the sealed class excludes them one at a time,
and once all are excluded nothing is left, so this applies to `if` and `isinstance`
as much as to `match`:

```by
def f(shape: Shape):
    if isinstance(shape, Circle):
        reveal_type(shape)  # Circle
    elif isinstance(shape, Square):
        reveal_type(shape)  # Square & not Circle
    else:
        reveal_type(shape)  # Never
```

a subclass in another module counts, so adding one can make a `match` elsewhere in
the workspace non-exhaustive, which is the point

a sealed class cannot be instantiated, since an instance of the class itself would
be a value no `match` over its subclasses covers:

```by
Shape()  # error: [call-non-callable]
```

a class created by calling `type()` cannot join the hierarchy, since the class
statements are what close it:

```by
Triangle = type("Triangle", (Shape,), {})  # error: [subclass-of-sealed-class]
```

## `__sealed_members__`

each sealed class gains a `__sealed_members__` attribute: a tuple of its direct
subclasses. the tuple starts empty, right after the sealed class's own statement,
and each subclass is added right after its own class statement, in the block it is
written in, as above. so the tuple holds the subclasses whose statements have run,
in the order they ran: one written under an `if` that is not taken, in a loop, in a
function not yet called, or in a module not yet loaded, is in it once its statement
has run. [imports are lazy](lazy-imports.md), so a module that is imported but that
nothing has read from yet has not added its subclasses

```by
sealed class Shape
class Circle(Shape)
class Square(Shape)

reveal_type(Shape.__sealed_members__)  # (*: type[Circle | Square])
```

the checker does not know the tuple's length, since which subclass statements have
run is not known statically, so it types it as any number of the subclasses

only direct subclasses are members: a subclass of a subclass is not listed

a subclass written in a `.py` file is a member for the checker, but python runs its
class statement untranspiled, so it never reaches the tuple. the same goes for a
subclass in another module under [`by compile`](../development/compilation/index.md), which
transpiles each module's interpreted half on its own

### subclasses in other modules

a subclass whose sealed class is bound anywhere other than its own scope adds itself
through a helper that reads the sealed classes off its bases, so it makes no
difference how the base is spelled or imported:

```by
# circle.by
from shapes import Shape as S

class Circle(S): ...
```

transpiles to:

```python
from shapes import Shape as S

class Circle(S): ...
_by_sealed_member(Circle)
```

### sealed classes in other scopes

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

the workspace of a first-party class is every first-party module, and that of a
class in a dependency is every module of its top-level package. a class outside it
is an error, since that is a value a `match` over the hierarchy would not expect:

```by
from some_dependency import Shape

class Circle(Shape): ...  # error: [subclass-of-sealed-class]
```

so is one in a file that is no module of the workspace, such as a script whose name
is not an identifier

## transpilation

`sealed` is a forward-only transform: the modifier keyword is stripped and the
`__sealed_members__` statements are inserted. there is no reverse transform, so
round-tripping python back to basedpython does not recover the `sealed` keyword
