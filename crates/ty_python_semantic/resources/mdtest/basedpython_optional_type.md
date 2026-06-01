# basedpython: `T?` optional type

`T?` in a type position is the optional type `T | None`, surface syntax for `Optional[T]`.

```toml
[environment]
python-version = "3.12"
```

## bare optional annotation

```by
def f(x: int?) -> None:
    reveal_type(x)  # revealed: int | None
```

## optional in a return annotation

```by
def f() -> int?:
    return None

reveal_type(f())  # revealed: int | None
```

## optional parameter narrows like a union

```by
def f(x: int?) -> None:
    if x is not None:
        reveal_type(x)  # revealed: int
    else:
        reveal_type(x)  # revealed: None
```

## optional inside a generic

```by
def f(xs: list[int?]) -> None:
    reveal_type(xs[0])  # revealed: int | None
```

## optional of a union flattens

```by
def f(x: int | str?) -> None:
    reveal_type(x)  # revealed: int | str | None
```

## double optional is a distinct wrapped type

a single `T?` is the lossless union `T | None`, but a nested optional cannot collapse that way (the
outer- and inner-`None` states would merge). so `int??` is a distinct wrapped type, rendered in `?`
notation, and `int?? != int | None`

```by
def g() -> int??:
    return None

reveal_type(g())  # revealed: int??
```

each extra layer adds another `?`:

```by
def h() -> int???:
    return None

reveal_type(h())  # revealed: int???
```

## force-unwrap `!` peels one optional layer

`expr!` removes one layer of optionality: a wrapped optional yields the next layer in, and a plain
`T | None` yields the present value `T`

```by
def g() -> int??:
    return None

result = g()
reveal_type(result)  # revealed: int??
reveal_type(result!)  # revealed: int | None
reveal_type(result!!)  # revealed: int
```

## propagate `^` peels one optional layer

`expr^` unwraps the present value (early-returning the absent value from the enclosing function), so
its type is the unwrapped value — the same peel as `!`

```by
def f() -> int?:
    return None

def g() -> int?:
    x = f()^
    reveal_type(x)  # revealed: int
    return x
```

## `^` / `!` on a result-like union peels the error arm

a result-like union (`T | E`, the error arm a `BaseException` subtype) is the unwrapped shape of a
`T ? E` result. `^` and `!` strip the exception arm, leaving the value type — the transpiler lowers
the guard to `isinstance(_, BaseException)` rather than `is None`

```by
def f() -> int | TypeError:
    return 1

def m() -> int | TypeError:
    x = f()^
    reveal_type(x)  # revealed: int
    return x

def n(r: str | ValueError) -> str | ValueError:
    reveal_type(r!)  # revealed: str
    return r
```

a union mixing both an error arm and `None` peels both:

```by
def p(r: int | None | TypeError) -> int | None | TypeError:
    reveal_type(r!)  # revealed: int
    return r
```

## `Some` is magically available

`Some` is the present-case optional constructor. It has no runtime definition in real Python — the
transpiler lowers `Some(x)` to the injected `Optional(x)` wrapper — so ty resolves it magically in
basedpython files (no import, not in any stub) rather than reporting an unresolved reference

```by
a = Some(None)
b = Some(1)
```

it takes exactly one value, so a missing or extra argument is an error:

```by
# error: [missing-argument]
a = Some()
# error: [too-many-positional-arguments]
b = Some(1, 2)
```

a local binding still shadows it:

```by
Some = 3
reveal_type(Some)  # revealed: 3
```
