## What it does

Checks for a parameter list that repeats `_` in a shape the lowering has no python spelling for, and
for a read of `_` that may be one of those parameters.

## Why is this bad?

Python refuses two parameters of one name, so every `_` after the first is given a name of its own.
That name is the lowering's rather than the author's, so a call reaches the parameter by position
alone: it is positional-only. Where that cannot be written, or would change a parameter the author
named, the lowering refuses the definition. Reported here, the refusal arrives while the file is
being checked rather than when it is transpiled.

## Examples

Python's `/` makes every parameter before it positional-only, so a named parameter ahead of a
repeated `_` would stop being reachable by keyword:

```by
# error: [invalid-repeated-underscore]
def pair(a: int, _: int, b: int, _: int) -> int:
    return a + b
```

Writing the `/` says so, and is accepted:

```by
def explicit(a: int, _: int, b: int, _: int, /) -> int:
    return a + b
```

A repeated `_` after `*` is keyword-only, and only a keyword reaches it:

```by
# error: [invalid-repeated-underscore]
def keyword(_: int, *, _: int) -> None: ...
```

Python before 3.8 has no `/` at all, so a module targeting it cannot make a repeated `_`
positional-only, and every definition that needs the `/` is refused there.

A method that overrides one takes the names the overridden method gives those positions. A name the
body reads from an enclosing scope would then read the parameter instead. Refused, the override
keeps no names a caller of the base could pass by keyword, which is reported too:

```by
x = 1

class Base:
    def f(self, x: int, y: int) -> int:
        return x + y

class Override(Base):
    # error: [invalid-repeated-underscore]
    # error: [invalid-method-override]
    override def f(self, _: int, _: int) -> int:
        return x
```

Python binds `_` to only one of the parameters, the first, while ty reads a body as binding each `_`
in turn. A read of `_` would mean a different parameter to each, so it is reported, wherever the
parameters may be the binding it reads — in the body, in a nested function, lambda or comprehension,
and in `_ += 1`:

```by
def first(_: int, _: str) -> str:
    # error: [invalid-repeated-underscore]
    return _
```

A read that only a binding the body makes reaches reads that binding, and `del _` reads no value:

```by
def rebound(_: int, _: int) -> int:
    _ = 5
    return _
```
