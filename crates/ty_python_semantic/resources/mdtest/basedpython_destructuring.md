# basedpython: destructuring

A pattern can bind wherever a name can: the `let` statement, a `for` target, a `with` item, a
parameter. Each is a single `match` case in disguise, so it binds and narrows exactly like one, and
it must match every value of the type it destructures — a pattern that may not match leaves its
captures unbound.

## The `let` statement

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def f(p: Point):
    let Point(x, y) := p
    reveal_type(x)  # revealed: int
    reveal_type(y)  # revealed: int
```

## A `let` narrows its subject

```by
def f(v: int | str):
    let int(n) := v else:
        raise ValueError
    reveal_type(n)  # revealed: int
    reveal_type(v)  # revealed: int
```

## A refutable `let` needs an `else` block

```by
def f(v: int | str):
    # error: [refutable-destructuring] "This pattern may not match `int | str`, which would leave its captures unbound"
    let int(n) := v
    reveal_type(n)  # revealed: int
```

## The `else` block has to diverge

Control falling out of the block reaches the captures the pattern did not bind.

```by
def f(v: int | str) -> int:
    # error: [refutable-destructuring] "The `else` block of a `let` has to diverge: what follows it needs the pattern's captures"
    let int(n) := v else:
        print("not an int")
    # error: [possibly-unresolved-reference] "Name `n` used when possibly not defined"
    return n
```

## Any way of leaving the block will do

```by
def f(v: int | str) -> int:
    let int(n) := v else:
        return 0
    return n

def g(v: int | str) -> int:
    let int(n) := v else:
        raise ValueError
    return n

def h(values: list[int | str]) -> None:
    for value in values:
        let int(n) := value else:
            continue
        print(n)
```

## A destructuring `for` target

```by
def f(pairs: list[tuple[int, str]]):
    for (number, text) in pairs:
        reveal_type(number)  # revealed: int
        reveal_type(text)  # revealed: str
```

## A `for` target that may not match

```by
class Shape: ...

class Circle(Shape):
    __match_args__ = ("radius",)

    def __init__(self, radius: int):
        self.radius = radius

def f(shapes: list[Shape]):
    # error: [refutable-destructuring]
    for Circle(radius) in shapes:
        reveal_type(radius)  # revealed: int
```

## A destructuring `with` item

```by
from typing import ContextManager

class Handle:
    __match_args__ = ("fd",)

    def __init__(self, fd: int):
        self.fd = fd

def f(cm: ContextManager[Handle]):
    with cm as Handle(fd):
        reveal_type(fd)  # revealed: int
```

## A destructuring parameter

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def f(Point(x, y): Point):
    reveal_type(x)  # revealed: int
    reveal_type(y)  # revealed: int
```

## A destructuring parameter is positional-only

Its name is a binder the source never wrote, so there is nothing to pass by keyword.

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def f(Point(x, y): Point) -> int:
    return x + y

f(Point(1, 2))
```

## A mapping pattern

```by
from typing import TypedDict

class Config(TypedDict):
    name: str
    size: int

def f(config: dict[str, int]):
    let {"size": size} := config else:
        raise KeyError
    reveal_type(size)  # revealed: int
```

## A tuple pattern

```by
def f(pair: tuple[int, str]):
    let (number, text) := pair
    reveal_type(number)  # revealed: int
    reveal_type(text)  # revealed: str
```

## Captures outlive the statement

```by
def f(pair: tuple[int, str]):
    let (number, text) := pair
    print(number, text)
```

## `let` is not a keyword

`let` introduces a destructuring only when a whole pattern followed by `:=` comes after it.

```by
let = 5
reveal_type(let)  # revealed: 5

let x = 1
reveal_type(x)  # revealed: 1
```

## A gradual subject is not blamed

Code that never said what it holds cannot be shown to match or not to match, so there is nothing to
report.

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def unannotated(cm, points) -> None:
    let Point(x, y) := points[0]

    for Point(a, b) in points:
        print(a, b)

    with cm as Point(c, d):
        print(c, d)

from typing import Any

def gradual(value: Any) -> None:
    let Point(x, y) := value
```

## A diverging `else` can be a call that never returns

```by
from typing import NoReturn

def bail() -> NoReturn:
    raise ValueError

def f(v: int | str) -> int:
    let int(n) := v else:
        bail()
    return n
```

## A destructuring parameter takes no keyword argument

Its binder is not a name a call site can write, so the diagnostic names the position instead.

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def f(Point(x, y): Point) -> int:
    return x + y

# error: [unknown-argument]
# error: [missing-argument] "No argument provided for required parameter 1 of function `f`"
f(x=Point(1, 2))
```

## Several binders in one header

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def f(Point(a, b): Point, Point(c, d): Point) -> int:
    return a + b + c + d

def g(first, second) -> None:
    with first as Point(a, b), second as Point(c, d):
        print(a, b, c, d)
```

## A destructuring parameter is not valid in an `init(...)` shorthand

Every parameter of an `init(...)` becomes a field of the same name, and a pattern has no name to
make one of.

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

class Box:
    # error: [invalid-syntax] "A destructuring parameter is not valid in an `init(...)` shorthand: it has no name to make a field of"
    init(Point(x, y): Point)
```

## `=` is not the binding operator

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def f(p: Point) -> None:
    # error: [invalid-syntax] "A destructuring `let` binds with `:=`, not `=`"
    let Point(x, y) = p
```
