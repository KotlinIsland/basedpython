# basedpython: pattern-matching `if let`

`if let <pattern> := <subject>:` takes the clause when the pattern matches the subject, binding the
pattern's captures. It is a single `match` case wearing an `if`, so it narrows and binds exactly
like one.

## Captures are bound in the clause body

```by
def f(v: int | str):
    if let int(n) := v:
        reveal_type(n)  # revealed: int
```

## The subject is narrowed inside the clause

```by
def f(v: int | str):
    if let int(n) := v:
        reveal_type(v)  # revealed: int
```

## The subject is narrowed in the `else` branch

```by
def f(v: int | str):
    if let int(n) := v:
        pass
    else:
        reveal_type(v)  # revealed: str
```

## A failed match falls through to the following clauses

```by
def f(v: int | str | None):
    if let int(n) := v:
        reveal_type(n)  # revealed: int
    elif v is None:
        reveal_type(v)  # revealed: None
    else:
        reveal_type(v)  # revealed: str
```

## `elif let` clauses

```by
def f(v: int | str | None):
    if v is None:
        reveal_type(v)  # revealed: None
    elif let int(n) := v:
        reveal_type(n)  # revealed: int
    else:
        reveal_type(v)  # revealed: str
```

## Captures leak past the clause

Like the captures of a `match` case (and like a walrus binding), a capture stays bound after the
statement — possibly unbound when the pattern did not match.

```by
def f(v: int | str):
    if let int(n) := v:
        pass
    # error: [possibly-unresolved-reference]
    reveal_type(n)  # revealed: int
```

## Sequence patterns

```by
def f(pair: tuple[int, str]):
    if let (a, b) := pair:
        reveal_type(a)  # revealed: int
        reveal_type(b)  # revealed: str
```

## Mapping patterns

```by
def f(v: dict[str, int]):
    if let {"key": value} := v:
        reveal_type(value)  # revealed: int
```

## `as` patterns

```by
def f(v: int | str):
    if let int() as n := v:
        reveal_type(n)  # revealed: int
```

## Or patterns

```by
def f(v: int | str | bytes):
    if let int(n) | str(n) := v:
        reveal_type(n)  # revealed: int | str
```

## Class patterns with keyword sub-patterns

```by
class Point:
    x: int
    y: str

def f(v: Point | None):
    if let Point(x=px, y=py) := v:
        reveal_type(px)  # revealed: int
        reveal_type(py)  # revealed: str
```

## The subject expression is still inferred

```by
def f(v: int | str):
    if let int(n) := v.bad:  # error: [unresolved-attribute]
        reveal_type(n)  # revealed: Unknown & int
```

## A wildcard pattern always matches

```by
def f(v: int | str):
    if let _ := v:
        reveal_type(v)  # revealed: int | str
```

## Nested `if let`

```by
def f(v: int | str, w: int | str):
    if let int(n) := v:
        if let str(s) := w:
            reveal_type(n)  # revealed: int
            reveal_type(s)  # revealed: str
```

## The subject is not tested for truthiness

A subject whose `__bool__` is unusable is fine — the clause matches a pattern, it never converts the
subject to `bool`.

```by
class NoBool:
    __bool__: int = 3

def f(v: NoBool):
    if let NoBool() := v:
        reveal_type(v)  # revealed: NoBool
```

## An unparenthesized sequence pattern

```by
def f(pair: tuple[int, str]):
    if let a, b := pair:
        reveal_type(a)  # revealed: int
        reveal_type(b)  # revealed: str
```

## Inside a loop

A capture bound by a clause in a loop body is visible on the next iteration.

```by
def f(items: list[int | str]):
    seen: int = 0
    while items:
        if let int(n) := items.pop():
            reveal_type(n)  # revealed: int
            seen = n
    reveal_type(seen)  # revealed: int
```

## A walrus subject

```by
def source() -> int | str:
    return 1

if let int(n) := (v := source()):
    reveal_type(n)  # revealed: int
    reveal_type(v)  # revealed: int
```

## `Some` is not a class, so it is not a pattern

`Some(x)` builds an optional rather than wrapping it in a class, so it cannot stand in pattern
position. Peeling an optional matches the type it wraps instead.

```by
def f(opt: int?):
    # error: [invalid-match-pattern]
    if let Some(x) := opt:
        pass
```

## Peeling an optional

```by
def f(opt: int?):
    if let int(x) := opt:
        reveal_type(x)  # revealed: int
    else:
        reveal_type(opt)  # revealed: None
```

## An enum variant

```by
enum class Shape:
    case Circle(r: int)
    case Square(side: int)

def f(shape: Shape):
    if let Shape.Circle(r) := shape:
        reveal_type(r)  # revealed: int
    else:
        reveal_type(shape)  # revealed: Square
```

## Consecutive `elif let` clauses

```by
def f(v: int | str | bytes):
    if let int(n) := v:
        reveal_type(n)  # revealed: int
    elif let str(s) := v:
        reveal_type(s)  # revealed: str
    elif let bytes(b) := v:
        reveal_type(b)  # revealed: bytes
    else:
        reveal_type(v)  # revealed: Never
```

## In a class body

```by
class C:
    v: int | str = 1
    if let int(k) := v:
        w = k

reveal_type(C.w)  # revealed: int
```

## `let` is still an ordinary name

`let` only introduces a pattern when the clause really is `let <pattern> := <subject>`.

```by
def f() -> int | None:
    return 1

if let := f():
    reveal_type(let)  # revealed: int & not AlwaysFalsy
```
