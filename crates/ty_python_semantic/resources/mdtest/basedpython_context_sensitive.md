# context-sensitive resolution

where an expression's expected type is known, a bare name that resolves to nothing else is looked up
as a member of that type: `a: Color = Red` means `Color.Red`, the way kotlin and swift resolve an
enum entry against the expected type. only *enum* members answer — a python enum's members, a based
enum's unit variants, and a based enum's payload variant classes.

it is the last resolution fallback, so it is purely additive: a name bound anywhere in the lexical
chain, or a builtin, keeps its ordinary meaning.

## a based enum's unit variant

```by
enum class Color:
    case Red, Green, Blue

a: Color = Red
reveal_type(a)  # revealed: Color.Red
```

## a python enum's member

```by
from enum import Enum

class Color(Enum):
    RED = 1
    GREEN = 2

a: Color = RED
reveal_type(a)  # revealed: Color.RED
```

## a payload variant constructs

the callee of a call resolves against the call's own expected type, so a payload variant is reached
the same way its unit siblings are

```by
enum class Shape:
    case Circle(radius: float)
    case Square(side: float)
    case Empty

s: Shape = Circle(2.0)
reveal_type(s)  # revealed: Circle
e: Shape = Empty
reveal_type(e)  # revealed: Shape.Empty
```

## call arguments and returns

every position with an expected type resolves the same way

```by
enum class Color:
    case Red, Green

def paint(c: Color) -> None: ...

paint(Red)

def favourite() -> Color:
    return Green
```

## collection elements, defaults, and class bodies

```by
enum class Color:
    case Red, Green, Blue

xs: list[Color] = [Red, Blue]
reveal_type(xs)  # revealed: list[Color]

def paint(c: Color = Green) -> None: ...

class Box:
    fill: Color = Blue
    reveal_type(fill)  # revealed: Color.Blue
```

## an optional or wider union still resolves

a union offers each of its elements' members, so an optional enum resolves too

```by
enum class Color:
    case Red, Green

a: Color | None = Red
reveal_type(a)  # revealed: Color.Red
b: Color | int = Green
reveal_type(b)  # revealed: Color.Green
```

## an ordinary binding wins

context-sensitive resolution is reached only when the name resolves to nothing else

```by
enum class Color:
    case Red, Green

Red = 1
a: Color = Red  # error: [invalid-assignment]
reveal_type(a)  # revealed: Color
```

## the diagnostic names the ambiguity

the qualified spelling always works, so an `unresolved-reference` on a name the expected type *does*
declare says which rule kept it from resolving

```by
enum class Color:
    case Red, Green

enum class Paint:
    case Red, Blue

a: Color | Paint = Red  # snapshot
```

```snapshot
error[unresolved-reference]: Name `Red` used when not defined
 --> src/mdtest_snippet.by:7:20
  |
7 | a: Color | Paint = Red  # snapshot
  |                    ^^^
  |
info: `Color` and `Paint` both declare `Red`: write it qualified
```

## the diagnostic names the shadowing binding

```by
enum class Color:
    case Red, Green

b: Color = Green  # snapshot
Green = 1
```

```snapshot
error[unresolved-reference]: Name `Green` used when not defined
 --> src/mdtest_snippet.by:4:12
  |
4 | b: Color = Green  # snapshot
  |            ^^^^^
  |
info: `Color` declares `Green`, but this scope binds `Green` itself: write `Color.Green`
```

## a later binding in the same scope wins too

the fallback is reached wherever the name is unbound *in flow*, but the transpiler qualifies a name
no scope binds at all. a scope that binds the name later therefore takes it back here as well, so
the emitted python cannot read a name before its assignment

```by
enum class Color:
    case Red, Green

# error: [unresolved-reference]
a: Color = Red
Red = 1
```

## a deleted binding stays deleted

```by
enum class Color:
    case Red, Green

Red = 1
del Red
# error: [unresolved-reference]
a: Color = Red
```

## a generic enum

a variant of a generic enum resolves against the specialized expected type

```by
enum class Tree[T]:
    case Leaf
    case Node(value: T)

a: Tree[int] = Leaf
reveal_type(a)  # revealed: Tree.Leaf
b: Tree[int] = Node(1)
reveal_type(b)  # revealed: Node[int]
```

## an enum imported from another module

`colours.by`:

```by
enum class Color:
    case Red, Green
```

`main.by`:

```by
from colours import Color

a: Color = Red
reveal_type(a)  # revealed: Color.Red
```

## an unknown member is still unresolved

```by
enum class Color:
    case Red, Green

# error: [unresolved-reference]
a: Color = Blue
```

## a non-enum context resolves nothing

only enum members answer; an ordinary class's attributes are not in scope unqualified

```by
class C:
    x = 1
    def f(self) -> None: ...

# error: [unresolved-reference]
a: C = f
```

## an ambiguous context resolves nothing

when two enums in the expected type declare the same name, neither is chosen

```by
enum class Color:
    case Red, Green

enum class Paint:
    case Red, Blue

# error: [unresolved-reference]
a: Color | Paint = Red
```

## the enum must be nameable

the transpiler qualifies the resolved name with the enum's own name, so an enum that cannot be
spelled here does not resolve

`colors.by`:

```by
enum class Color:
    case Red, Green
```

`main.by`:

```by
from colors import Color as C

a: C = Red  # snapshot
b: C = C.Red  # this always works
```

```snapshot
error[unresolved-reference]: Name `Red` used when not defined
 --> src/main.by:3:8
  |
3 | a: C = Red  # snapshot
  |        ^^^
  |
info: `Red` is a member of `Color`, which is not in scope here under that name
```

## match patterns are unaffected

a bare name in a `case` pattern is a capture, not a load, and keeps that meaning

```by
enum class Color:
    case Red, Green

def f(c: Color) -> None:
    match c:
        case Red:
            reveal_type(Red)  # revealed: Color
```

## the enum's own body

a member is reachable unqualified inside the enum that declares it

```by
from enum import Enum

class Color(Enum):
    RED = 1
    GREEN = 2

    def default(self) -> Color:
        return RED
```

## before the enum is declared

the expected type decides the lookup, not the order of declarations

```by
def favourite() -> Color:
    return Red

enum class Color:
    case Red, Green
```
