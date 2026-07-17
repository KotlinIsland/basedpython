# basedpython: `cast?` checked cast

In basedpython, `<value> cast? <type>` is a *checked* cast: at runtime it yields the value when
`isinstance(value, type)` holds and `None` otherwise, so its type is `type | None`. The transpiler
lowers it to a `_checked_cast(value, type)` helper call.

## simple checked cast

```by
def f(a: object):
    b = a cast? int
    reveal_type(b)  # revealed: int | None
```

## checked cast to union

```by
def f(a: object):
    b = a cast? int | str
    reveal_type(b)  # revealed: int | str | None
```

## checked cast in call argument

```by
def g(x: int | None) -> int | None:
    return x

def f(a: object):
    reveal_type(g(a cast? int))  # revealed: int | None
```

## the value operand is a value position

The value keeps its own type; only the result is `type | None`.

```by
def f(a: str):
    b = a cast? int
    reveal_type(a)  # revealed: str
    reveal_type(b)  # revealed: int | None
```

## not valid in `.py` files

`cast?` is basedpython-only. A `.py` file using it gets a parse error from the parser.

```py
a = 1
# error: [invalid-syntax] "`cast?` (checked cast) keyword is not valid in .py files"
b = a cast? int
```
