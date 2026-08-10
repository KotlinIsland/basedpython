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
info: `Red` is a member of `Color`, which is not in scope here under that name
```

## a case pattern resolves against its subject

a `case` pattern's expected type is the subject's, so a bare name there is offered to the same
resolution. python reads such a name as a capture, and it still is wherever it names no member

```by
enum class Color:
    case Red, Green

def f(c: Color) -> None:
    match c:
        case Red:
            reveal_type(c)  # revealed: Color.Red
```

## a resolved case name binds nothing

it is a value pattern, so there is no capture — exactly as if it had been written `Color.Red`

```by
enum class Color:
    case Red, Green

def f(c: Color) -> None:
    match c:
        case Red:
            # error: [unresolved-reference]
            reveal_type(Red)  # revealed: Unknown
```

## a python enum's member in a case pattern

```by
from enum import Enum

class Color(Enum):
    RED = 1
    GREEN = 2

def f(c: Color) -> None:
    match c:
        case RED:
            reveal_type(c)  # revealed: Color.RED
```

## an enum match is exhaustive

each case removes its member from what reaches the next, so nothing is left over and the function
needs no fallback return

```by
enum class Color:
    case Red, Green, Blue

def f(c: Color) -> int:
    match c:
        case Red:
            return 1
        case Green:
            return 2
        case Blue:
            return 3
```

## the last case resolves like the first

by the last case of an exhaustive match nothing of the enum is left to resolve against, so the name
is resolved against the subject the case was written for rather than against what reached it

```by
enum class Color:
    case Red, Green

def f(c: Color) -> int:
    match c:
        case Red:
            return 1
        case Green:
            reveal_type(c)  # revealed: Color.Green
            return 2
```

## an alternative resolves too

```by
enum class Color:
    case Red, Green, Blue

def f(c: Color) -> None:
    match c:
        case Red | Green:
            reveal_type(c)  # revealed: Color.Red | Color.Green
        case Blue:
            reveal_type(c)  # revealed: Color.Blue
```

## a payload variant destructures

the class of a `case` pattern is resolved the same way, so a variant can be matched and unpacked
without naming the enum

```by
enum class Shape:
    case Circle(radius: int)
    case Empty

def f(s: Shape) -> int:
    match s:
        case Circle(r):
            reveal_type(r)  # revealed: int
            return r
        case Empty:
            return 0
```

## an ordinary binding keeps the capture

a name the scope binds anywhere is python's capture, whatever the subject declares

```by
enum class Color:
    case Red, Green

def f(c: Color) -> None:
    Red = 1
    match c:
        case Red:
            reveal_type(Red)  # revealed: Color
```

## a capturing name before a later case is reported

python rejects this as a syntax error. basedpython cannot tell a capture from a member without
types, so the checker reports it instead

```by
enum class Color:
    case Red, Green

def f(c: Color) -> int:
    match c:
        # error: [invalid-match-pattern] "name capture `nope` makes remaining patterns unreachable"
        case nope:
            return 1
        case Green:
            return 2
```

## the diagnostic names why the case captured

the same rules that keep an assignment from resolving keep a case pattern from resolving, and the
report says which one did it — the qualified spelling always works

`palette.by`:

```by
enum class Color:
    case Red, Green
```

`main.by`:

```by
from palette import Color as C

def f(c: C) -> int:
    match c:
        # snapshot: invalid-match-pattern
        case Red:
            return 1
        case _:
            return 0
```

```snapshot
error[invalid-match-pattern]: name capture `Red` makes remaining patterns unreachable
 --> src/main.by:6:14
  |
6 |         case Red:
  |              ^^^
info: `Red` is a member of `Color`, which is not in scope here under that name
```

## a capturing name in the last case is fine

```by
enum class Color:
    case Red, Green

def f(c: Color) -> int:
    match c:
        case Red:
            return 1
        case other:
            reveal_type(other)  # revealed: Literal[Color.Green]
            return 2
```

## a capturing alternative is reported

each alternative of an `or` pattern has to bind the same names, and a capture binds one its siblings
do not

```by
enum class Color:
    case Red, Green

def f(c: Color) -> int:
    match c:
        # error: [invalid-match-pattern] "alternative patterns bind different names"
        case Red | nope:
            return 1
```

## the wildcard is still irrefutable

`_` means the same thing whatever the subject is, so it is still python's own syntax error

```by
enum class Color:
    case Red, Green

def f(c: Color) -> int:
    match c:
        case _:  # error: [invalid-syntax] "wildcard makes remaining patterns unreachable"
            return 1
        case Green:
            return 2
```

## a nested bare name is a capture

only a name matched against the subject itself has an expected type; inside a class or sequence
pattern a bare name captures the part it is aligned with, as in python

```by
enum class Color:
    case Red, Green

def f(pair: tuple[Color, Color]) -> None:
    match pair:
        case [Red, second]:
            reveal_type(Red)  # revealed: Color
            reveal_type(second)  # revealed: Color
```

## a guarded case resolves too

```by
enum class Color:
    case Red, Green

def f(c: Color, ready: bool) -> None:
    match c:
        case Red if ready:
            reveal_type(c)  # revealed: Color.Red
```

## an `if let` clause resolves too

```by
enum class Color:
    case Red, Green

def f(c: Color) -> None:
    if let Red := c:
        reveal_type(c)  # revealed: Color.Red
```

## a binder's bare name always captures

a `for` target, a `with` item, a parameter and an `else`-less `let` all have to bind — their pattern
is not allowed to fail — so a bare name there is never a value

```by
enum class Color:
    case Red, Green

def f(c: Color) -> None:
    let Red := c
    reveal_type(Red)  # revealed: Color
```

## a non-enum subject resolves nothing

```by
def f(x: int) -> None:
    match x:
        case Red:
            reveal_type(Red)  # revealed: int
```

## python files are unaffected

```py
from enum import Enum

class Color(Enum):
    RED = 1
    GREEN = 2

def f(c: Color) -> None:
    match c:
        case RED:
            reveal_type(RED)  # revealed: Color
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

## names that resolve with no binding behind them

this fallback runs at the very *end* of ty's name-resolution chain, so it gates only on the lexical
scope: anything an earlier entry in that chain resolves has already claimed the name before the enum
member is ever looked for. that is why it uses the narrower of the two shared name-fallback gates —
the wider one, which the implicit-receiver and django-lookup rules need, would take these names back
from the enum for a second time and take one more with them

### an implicit `typing` name keeps its meaning

```by
enum class Filter:
    case Any, All

# error: [invalid-assignment] "Object of type `<special-form 'typing.Any'>` is not assignable to `Filter`"
a: Filter = Any
```

### `Some` keeps its meaning

```by
enum class Option:
    case Some, Nothing

# error: [invalid-assignment] "Object of type `[_SomeT](value: _SomeT, /) -> _SomeT?` is not assignable to `Option`"
a: Option = Some
```

### `Character` outside a type expression does not

`Character` is implicitly available only *in* a type expression, so in a value position no earlier
entry claims it and the enum member answers. this is the one name the two gates observably disagree
about

```by
enum class Token:
    case Character, Digit

a: Token = Character
reveal_type(a)  # revealed: Token.Character
```
