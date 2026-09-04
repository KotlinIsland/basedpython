# basedpython-ui: a `context` parameter may precede a trailing callback

A `context` parameter is filled by keyword, so nothing may follow it that an explicit positional
argument could land on. The one parameter that never takes a positional argument is the callback a
trailing block fills — it is always passed by keyword, as the callee's last parameter — so a
component can declare both a `context` parameter and a content block.

## the last parameter may be a callable

```by
def Card(title: str, context theme: str, once content: () -> None):
    content()

context theme = "dark"

Card("x"):
    pass

def body() -> None: ...

Card("y", content=body)
```

## the callable still binds the block by keyword

An earlier defaulted parameter keeps its default: the block goes to `content`, not to the next
positional slot.

```by
def Card(title: str = "untitled", context theme: str = "light", once content: () -> None = lambda: None):
    content()

context theme = "dark"

Card():
    pass
```

## a keyword-only parameter may follow too

A keyword-only parameter cannot take a positional argument at all, so the rule never had anything to
say about one — with or without the trailing-callback exemption.

```by
def Card(title: str, context theme: str, *, once content: () -> None):
    content()

context theme = "dark"

Card("x"):
    pass
```

## anything else after a `context` parameter is still rejected

A callable that is not the last parameter is not the trailing callback, and a non-callable last
parameter could take a positional argument.

```by
# error: [invalid-syntax] "parameter after a `context` parameter must also be `context`"
def f(context b: str, a: int): ...

# error: [invalid-syntax] "parameter after a `context` parameter must also be `context`"
def g(context b: str, cb: () -> None, a: int): ...  # error: [invalid-syntax]
```

## an unmarked callable is rejected too

The exemption is for the callback a trailing block fills, which the call passes by keyword. An
ordinary callable parameter can take a positional argument, and a positional argument written after
a `context` parameter would land on the `context` parameter instead — so a last parameter earns the
exemption only by carrying the `once` / `local` modifier that marks it a borrowed callback. A plain
callable that must follow a `context` parameter can still be written keyword-only.

```by
# error: [invalid-syntax] "parameter after a `context` parameter must also be `context`"
def Card(title: str, context theme: str, on_click: () -> None): ...

def Ok(title: str, context theme: str, *, on_click: () -> None): ...
```
