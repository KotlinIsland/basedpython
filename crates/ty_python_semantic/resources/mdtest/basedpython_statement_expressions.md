# basedpython: statement expressions

A compound statement can stand where a value is expected. Its type is the union of the values its
branches evaluate last, and it is an error if some path through it produces no value at all.

## `if`

```by
def f(c: bool):
    a = if c:
        1
    else:
        "two"
    reveal_type(a)  # revealed: 1 | "two"
```

## `elif` chains

```by
def f(n: int):
    a = if n == 0:
        "none"
    elif n == 1:
        "one"
    else:
        "many"
    reveal_type(a)  # revealed: "none" | "one" | "many"
```

## a branch may do work before its value

```by
def log(m: str): ...

def f(c: bool):
    a = if c:
        log("yes")
        1
    else:
        2
    reveal_type(a)  # revealed: 1 | 2
```

## a branch's value may be a call

A call is a standalone expression in the semantic index, so inferring one as a branch value has to
go through the standalone path rather than re-inferring it.

```by
def make() -> int:
    return 1

def f(c: bool):
    a = if c:
        make()
    else:
        "two"
    reveal_type(a)  # revealed: int | "two"
```

## a branch's value may be a comprehension

```by
def f(c: bool):
    a = if c:
        [x for x in [1, 2]]
    else:
        "two"
    reveal_type(a)  # revealed: list[int] | "two"
```

## a `match` branch's value may be a call

```by
def make() -> int:
    return 1

def f(n: int):
    a = match n:
        case 0:
            make()
        case _:
            "other"
    reveal_type(a)  # revealed: int | "other"
```

## an `if` without `else` is not exhaustive

```by
def f(c: bool):
    # error: [non-exhaustive-statement-expression] "this `if` expression can complete without producing a value"
    a = if c:
        1
```

## a branch that does not end in an expression is not exhaustive

```by
def f(c: bool):
    # error: [non-exhaustive-statement-expression] "this `if` expression can complete without producing a value"
    a = if c:
        1
    else:
        pass
```

## `match`

```by
def f(x: int | str):
    a = match x:
        case int():
            1
        case str():
            "s"
    reveal_type(a)  # revealed: 1 | "s"
```

## a `match` that does not cover the subject is not exhaustive

```by
def f(x: int | str):
    # error: [non-exhaustive-statement-expression] "this `match` expression can complete without producing a value"
    a = match x:
        case int():
            1
```

## a wildcard case always covers

```by
def f(x: object):
    a = match x:
        case int():
            1
        case _:
            0
    reveal_type(a)  # revealed: 1 | 0
```

## `for` yields through `break`

```by
def f(xs: list[int]):
    a = for x in xs:
        break x
    else:
        -1
    reveal_type(a)  # revealed: int
```

## a loop without `else` is not exhaustive

```by
def f(xs: list[int]):
    # error: [non-exhaustive-statement-expression] "this `for` expression can complete without producing a value"
    a = for x in xs:
        break x
```

## `while`

```by
def next_value() -> int | None: ...

def f():
    a = while True:
        v = next_value()
        if v is not None:
            break v
    else:
        0
    reveal_type(a)  # revealed: int
```

## a `break` in a nested loop targets that loop

The inner `break` leaves the inner loop, so it is not one of the outer loop's value positions.

```by
def f(rows: list[list[int]]):
    a = for row in rows:
        for cell in row:
            break
        break len(row)
    else:
        0
    reveal_type(a)  # revealed: int
```

## narrowing inside a branch applies to its value

```by
def f(x: int | None):
    a = if x is None:
        0
    else:
        x
    reveal_type(a)  # revealed: int
```

## bindings made inside a statement expression are visible after it

```by
def f(c: bool):
    a = if c:
        b = 1
        b
    else:
        b = 2
        b
    reveal_type(a)  # revealed: 1 | 2
    reveal_type(b)  # revealed: 1 | 2
```

## a nested compound statement in tail position supplies the branch's value

```by
def f(x: int | str, c: bool):
    a = if c:
        match x:
            case int():
                1
            case str():
                2
    else:
        3
    reveal_type(a)  # revealed: 1 | 2 | 3
```

## a diverging branch contributes no value

```by
def f(x: int | str):
    a = match x:
        case int():
            1
        case str():
            raise ValueError(x)
    reveal_type(a)  # revealed: 1
```

## `raise` is an expression of type `Never`

```by
def f(x: int | None) -> int:
    return x ?? raise ValueError()
```

```by
def f(x: int) -> int:
    a = x if x > 0 else raise ValueError(x)
    reveal_type(a)  # revealed: int
    return a
```

## `return` is an expression of type `Never`

```by
def f(x: int | None) -> int:
    a = x ?? return 0
    reveal_type(a)  # revealed: int
    return a
```

## a statement expression is not a type expression

```by
# error: [invalid-syntax] "a statement expression must be the tail of its statement"
# error: [invalid-type-form] "Statement expressions are not allowed in type expressions"
a: raise ValueError = 1
```

## a statement expression with a suite must be the whole value of its statement

```by
def f(c: bool):
    # error: [invalid-syntax] "a statement expression with a suite must be the whole value of its statement"
    a = 1 + if c:
        1
    else:
        2
```

## a diverging statement expression must still be in tail position

```by
def f() -> list[int]:
    # error: [invalid-syntax] "a statement expression must be the tail of its statement"
    return [raise ValueError()]
```

## `break` may only carry a value where something reads it

```by
def f(xs: list[int]) -> None:
    for x in xs:
        # error: [invalid-syntax] "`break` with a value must be inside a loop used as a statement expression"
        break x
```

A `break` in a loop nested inside a statement expression's loop leaves *that* loop, so it may not
carry one either.

```by
def f(rows: list[list[int]]) -> int:
    a = for row in rows:
        for cell in row:
            # error: [invalid-syntax] "`break` with a value must be inside a loop used as a statement expression"
            break cell
        break len(row)
    else:
        0
    return a
```

## a statement expression with a suite must be the first statement on its line

Its suite continues the line its statement starts on, so nothing may precede it there.

```by
def f(x: int) -> int:
    # error: [invalid-syntax] "a statement expression with a suite must be the first statement on its line"
    p = 1; q = match x:
        case _:
            2
    return p + q
```

## every branch diverging is `Never`, not a missing value

```by
def f(c: bool) -> int:
    a = if c:
        raise ValueError()
    else:
        raise TypeError()
    reveal_type(a)  # revealed: Never
    return a
```
