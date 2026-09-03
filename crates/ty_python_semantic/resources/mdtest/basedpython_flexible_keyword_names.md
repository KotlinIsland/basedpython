# basedpython: flexible keyword argument names

a keyword argument may be named by a dotted path or by a string literal, not only by a bare
identifier. the name is the path's text or the string's value, so a call can name a keyword python
has no way to spell.

## a name python cannot spell reaches the keyword-variadic parameter

`**kwargs` takes any name at all, so a key holding a dot or a slash lands there and is checked
against its declared type.

```by
def f(**kwargs: int) -> None: ...

f(foo.bar=1, "content-type"=2)

# error: [invalid-argument-type] "Expected `int`, found `"x"`"
f("content-type"="x")
```

## a quoted name binds the parameter it names

the quotes are surface syntax; a name a parameter already has binds that parameter, exactly as the
bare spelling would.

```by
def f(timeout: int) -> None: ...

f("timeout"=1)

# error: [invalid-argument-type] "Expected `int`, found `"x"`"
f("timeout"="x")
```

## a name no parameter has is still an unknown argument

a function without a keyword-variadic parameter has no room for one, and the diagnostic names the
key as written.

```by
def f(a: int) -> None: ...

# error: [unknown-argument] "Argument `a.b` does not match any known parameter of function `f`"
# error: [missing-argument]
f(a.b=1)
```

## a flexible name becomes a field of a keyword-variadic pack

a [keyword-variadic pack] is an ordered name-to-type mapping, and a flexible name is a field of it
like any other.

```by
class Headers[**Fields]:
    def __init__(self, **fields: **Fields) -> None: ...

reveal_type(Headers("content-type"="json", size=1))  # revealed: final Headers[content-type=str, size=int]
```

[keyword-variadic pack]: https://docs.basedpython.org/features/keyword-variadic/
