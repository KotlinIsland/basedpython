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

- **`match` patterns are unaffected.** a bare name in a `case` pattern is a
    capture, not a load, and keeps that meaning. match an enum member with its
    qualified form, as python requires:

    ```by
    match colour:
        case Color.Red: ...
        case _: ...
    ```

## transpiler output

the resolved name is emitted qualified — it has no runtime binding of its own:

```by
enum class Color:
    case Red, Green

a: Color = Red
```

```python
from __future__ import annotations
from enum import Enum, auto
class Color(Enum):
    Red = auto()
    Green = auto()

a: Color = Color.Red
```
