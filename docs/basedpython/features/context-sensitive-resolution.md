# context-sensitive resolution

where an expression's expected type is known, a bare name that resolves to
nothing else is looked up as a member of that type. an enum member can then be
written unqualified, the way kotlin and swift allow:

```by
enum class Color:
    case Red, Green, Blue

a: Color = Red  # Color.Red
```

the qualified spelling always works too — this is an additional way to reach a
member, never a replacement

## where it applies

anywhere the checker already knows what type an expression is expected to
produce:

```by
enum class Color:
    case Red, Green, Blue

a: Color = Red                       # annotated assignment
b: Color | None = Green              # an optional (or wider) union
c: list[Color] = [Red, Blue]         # element position

def paint(colour: Color) -> None: ...

paint(Red)                           # argument

def favourite() -> Color:
    return Blue                      # return
```

the *callee* of a call resolves against the call's own expected type, so a
[based enum](enums.md) payload variant is constructed the same way:

```by
enum class Shape:
    case Circle(radius: float)
    case Square(side: float)
    case Empty

s: Shape = Circle(2.0)
e: Shape = Empty
```

the expected type always comes from a *declaration* — an annotation, a
parameter, a declared return type. an expression whose type is merely inferred
from a peer supplies no context, so a comparison operand keeps its ordinary
meaning:

```by
enum class Color:
    case Red, Green

def f(c: Color) -> bool:
    return c == Red  # error: unresolved reference; write `Color.Red`
```

## what answers

only **enum members**: a python `enum.Enum` member, a based enum's unit variant,
and a based enum's payload variant class. an ordinary class's attributes stay
qualified — `x: C = f` does not find `C.f`

## the rules

- **ordinary resolution wins.** the lookup is the last fallback, reached only
    when the name resolves to nothing else. a name bound anywhere in the lexical
    chain, or a builtin, keeps its ordinary meaning, so no existing program
    changes behaviour:

    ```by
    enum class Color:
        case Red, Green

    Red = 1
    a: Color = Red  # error: `Red` is the `int`, not `Color.Red`
    ```

    *anywhere* is literal: a scope that binds the name takes it back even where
    the binding is not yet in flow, so a `Red = 1` **below** the use also wins
    (and is reported as a use before assignment, as it would be in python). the
    qualified form has no runtime binding to race, so `Color.Red` always works

- **ambiguity resolves to nothing.** when two enums in the expected type declare
    the same name, neither is chosen and the name stays unresolved:

    ```by
    enum class Color:
        case Red, Green

    enum class Paint:
        case Red, Blue

    a: Color | Paint = Red  # error: unresolved reference
    ```

- **the enum must be nameable.** the transpiler emits the qualified form, so the
    enum has to be reachable here under its own name. an enum imported under an
    alias, or reached through a module (`m.Color`), does not resolve:

    ```by
    from colours import Color as C

    a: C = Red     # error: unresolved reference
    b: C = C.Red   # always works
    ```

## match patterns

a `case` pattern's expected type is its subject's, so a bare name there resolves
the same way. python reads such a name as a capture — and it still is wherever it
names no member — but a member wins, and the case becomes the value pattern it
looks like:

```by
enum class Color:
    case Red, Green, Blue

def describe(c: Color) -> str:
    match c:
        case Red:
            return "warm"
        case Green | Blue:
            return "cool"
```

the match is exhaustive, each case narrows its subject, and no name is bound:
`Red` inside the first case is not in scope, exactly as if `Color.Red` had been
written.

a payload variant is matched and unpacked the same way, because the class of a
`case` pattern is resolved against the subject too:

```by
enum class Shape:
    case Circle(radius: float)
    case Square(side: float)

def area(s: Shape) -> float:
    match s:
        case Circle(r):
            return 3.14 * r * r
        case Square(a):
            return a * a
```

only a name matched against the subject *itself* is offered to the lookup —
through `|`, `and` and the left-hand side of `as`. inside a class, sequence or
mapping pattern a bare name captures the part it is aligned with, as in python:

```by
def f(pair: tuple[Color, Color]) -> None:
    match pair:
        case [Red, second]: ...  # both are captures
```

a binder whose pattern has to succeed — a `for` target, a `with` item, a
parameter, an `else`-less `let` — never resolves a bare name either: its whole
purpose is to bind.

### a capture where python wanted a test

python rejects a capture that makes later cases unreachable, or that binds names
its sibling alternatives do not. basedpython cannot tell a capture from a member
without types, so the checker reports both instead of the parser:

```by
enum class Color:
    case Red, Green

def f(c: Color) -> int:
    match c:
        case nope:  # error: name capture `nope` makes remaining patterns unreachable
            return 1
        case Green:
            return 2
```

## transpiler output

the resolved name is emitted qualified — it has no runtime binding of its own:

```by
enum class Color:
    case Red, Green

a: Color = Red

def f(c: Color) -> int:
    match c:
        case Red:
            return 1
        case Green:
            return 2
```

```python
from __future__ import annotations
from enum import Enum, auto
class Color(Enum):
    Red = auto()
    Green = auto()

a: Color = Color.Red

def f(c: Color) -> int:
    match c:
        case Color.Red:
            return 1
        case Color.Green:
            return 2
```
