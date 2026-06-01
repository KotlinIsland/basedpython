# basedpython: `dynamic` keyword

basedpython spells the dynamic type `dynamic` instead of importing `Any`. In a type expression a
bare `dynamic` resolves to `typing.Any`; the transpiler lowers it to `Any`. Outside a type
expression `dynamic` is an ordinary identifier.

## `dynamic` annotation is `Any`

```by
def f(x: dynamic) -> None:
    reveal_type(x)  # revealed: Any
```

## bare module-level `dynamic` annotation

A bare `dynamic` variable annotation resolves without an `unresolved-reference` error.

```by
x: dynamic = 1
reveal_type(x)  # revealed: Any
```

## `dynamic` return is `Any`

```by
def g() -> dynamic: ...

reveal_type(g())  # revealed: Any
```

## `dynamic` flows like `Any`

A `dynamic`-typed value is assignable to and from any type, with no import needed.

```by
def takes_int(n: int) -> None: ...

def use(x: dynamic) -> None:
    takes_int(x)  # accepted: `Any` is assignable to `int`
    n: int = x  # accepted: `Any` is assignable from anything
```

## `dynamic` nested in a generic

```by
def f(xs: list[dynamic]) -> None:
    reveal_type(xs)  # revealed: list[Any]
    reveal_type(xs[0])  # revealed: Any
```

## `dynamic` composes with union

```by
def f(a: dynamic | None) -> None:
    reveal_type(a)  # revealed: Any | None
```

## `dynamic` as a class base

Subclassing `dynamic` is the basedpython spelling of subclassing `Any`, and resolves without an
`unresolved-reference` error.

```by
class C(dynamic): ...

reveal_type(C())  # revealed: C
```

## value-position `dynamic` is an ordinary identifier

Outside a type expression `dynamic` carries no special meaning — it is a normal name.

```by
dynamic = 5
reveal_type(dynamic)  # revealed: 5
```

## a local binding shadows the keyword

When `dynamic` is bound in scope, the annotation uses that binding rather than `Any`.

```by
dynamic = int

def f(x: dynamic) -> None:
    reveal_type(x)  # revealed: int
```

## `.py` does not treat `dynamic` specially

In a plain Python file `dynamic` is not a keyword, so an unresolved `dynamic` annotation is an
error.

`mod.py`:

```py
def f(x: dynamic) -> None: ...  # error: [unresolved-reference]
```
