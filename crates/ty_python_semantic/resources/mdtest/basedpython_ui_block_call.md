# basedpython-ui: a call carrying a trailing block binds like an ordinary call

A statement-level `f(...):` block passes its suite as the call's last argument, and the rest of the
call is still a call: its `context` parameters are filled from the `context` declarations in scope,
and its arguments are conversion sites. A compose-style component declares both — a `context` theme
and a `once` content block — so the block form has to bind exactly like `f(...)` does.

## a `context` parameter is filled on a block-carrying call

```by
def Card(title: str, context theme: str, *, once content: () -> None):
    content()

context theme = "dark"

Card("x"):
    pass
```

## a missing `context` argument is still reported

```by
def Card(title: str, context theme: str, *, once content: () -> None):
    content()

Card("x"):  # error: [missing-context-argument]
    pass
```

## an explicit argument still wins over the declaration

```by
def Card(title: str, context theme: str, *, once content: () -> None):
    content()

context theme = "dark"

Card("x", theme="light"):
    pass
```

## a literal argument converts through `__of__` on a block-carrying call

The argument is inferred against its parameter, so the literal is a conversion site — the same
`__of__` that `Padding(8)` without a block resolves.

```by
class Dp:
    value: float = 0.0

    @classmethod
    def __of__(cls, value: int | float) -> Self:
        return cls()

def Padding(amount: Dp, *, once content: () -> None):
    content()

Padding(8):
    pass

Padding(8.5):
    pass
```

## a value the target cannot convert is still rejected

```by

class Dp:
    value: float = 0.0

    @classmethod
    def __of__(cls, value: int | float) -> Self:
        return cls()

def Padding(amount: Dp, *, once content: () -> None):
    content()

Padding("wide"):  # error: [invalid-argument-type]
    pass
```
