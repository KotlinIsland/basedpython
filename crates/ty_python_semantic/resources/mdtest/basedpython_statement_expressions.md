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

## `try`

The `try` block's value is what it evaluates last, and each handler supplies the value for the
exception it catches.

```by
def f(s: str):
    a = try:
        s.index("=")
    except ValueError:
        "no separator"
    reveal_type(a)  # revealed: int | "no separator"
```

## an `else` clause is what a completed `try` block produces

The `else` clause runs once the `try` block has completed, so it is the value on that path and the
`try` block's own last expression is not.

```by
def f(s: str):
    a = try:
        at = s.index("=")
    except ValueError:
        0
    else:
        at + 1
    reveal_type(a)  # revealed: int
```

## a `finally` clause produces no value

It runs on the way out of every path, including the ones that carry an exception past the statement,
so what it evaluates last is not what the statement produces.

```by
def f(s: str, log: list[str]):
    a = try:
        s.index("=")
    except ValueError:
        0
    finally:
        log.append(s)
    reveal_type(a)  # revealed: int
```

## a handler that does not end in an expression is not exhaustive

```by
def f(s: str):
    # error: [non-exhaustive-statement-expression] "this statement expression can complete without producing a value"
    a = try:
        s.index("=")
    except ValueError:
        pass
```

## a handler that raises contributes no value

```by
def f(s: str):
    a = try:
        s.index("=")
    except ValueError:
        raise TypeError(s)
    reveal_type(a)  # revealed: int
```

## a suite ends the statement it is written in

A suite runs to the end of its last line, taking with it the newline that would otherwise terminate
the statement the suite is written in. The line after it therefore begins a new statement, even when
that line opens with a token the expression parser would otherwise read as a continuation of the
value — here the `if` that would be a conditional expression anywhere else.

```by
import json

def f(text: str) -> object:
    parsed = try:
        json.loads(text)
    except json.JSONDecodeError:
        return None
    if not isinstance(parsed, dict):
        return None
    return parsed
```

The other continuations are held apart the same way: a call's `(`, a subscript's `[`, a binary
operator, and a walrus each start the next statement rather than extending the one the suite ended.

```by
def g(c: bool, xs: list[int]):
    a = if c:
        1
    else:
        2
    (xs).append(a)

    b = if c:
        1
    else:
        2
    [b]

    d = if c:
        1
    else:
        2
    -d

    e = if c:
        1
    else:
        2
    (f := e)
    reveal_type(f)  # revealed: 1 | 2
```

a comma is held apart too, so the line after a suite is a statement of its own rather than the tail
of a tuple. `g` is bound to the branch's value, and the `2,` below it is a statement in its own
right.

```by
def h(c: bool):
    g = if c:
        1
    else:
        2
    2, 3
    reveal_type(g)  # revealed: 1 | 2
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

## `continue` is an expression of type `Never`

A loop escape written where a value is expected leaves the loop when the value is missing, so what
the binding takes on is the type that survives the escape.

```by
def f(items: list[int | None]) -> int:
    total = 0
    for item in items:
        one = item ?? continue
        reveal_type(one)  # revealed: int
        total += one
    return total
```

## `break` is an expression of type `Never`

```by
def f(items: list[int | None]) -> int:
    total = 0
    for item in items:
        one = item ?? break
        reveal_type(one)  # revealed: int
        total += one
    return total
```

## a loop escape is diverging, so it needs no `else` to be exhaustive

`break` and `continue` produce no value, so a statement expression whose every branch is one of them
is `Never` rather than a value that went missing.

```by
def f(items: list[int]) -> None:
    for item in items:
        a = continue if item > 0 else break
        reveal_type(a)  # revealed: Never
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

## a compound statement's header is not a value position

A compound statement has no value of its own, so nothing in its header — a `for` iterable, an `if`
test, a base class — is the tail of anything.

```by
def f(xs: list[int] | None):
    # error: [invalid-syntax] "a statement expression must be the tail of its statement"
    for x in xs ?? raise ValueError():
        print(x)
```

## a rejected statement expression is dropped along with its suite

What the suite declares belongs to the scope the statement expression is written in, which is only
true where the position rule holds. So a rejected one keeps nothing but the range it was written at,
and the rest of the file is still checked.

```by
def f(n: int) -> int:
    # error: [invalid-syntax] "a statement expression with a suite must be the whole value of its statement"
    assert match n:
        case 1:
            v: int = 2
            v
        case _: 0
    return n
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

## a statement expression inside a trailing-lambda block

the block's suite becomes a synthesized function, and each of its statements owns its own value
position — the call the block trails is not it.

```by
def run(n: int = 1, cb: (int) -> None) -> None:
    cb(n)

run(1):
    v = if it > 0:
        1
    else:
        2
    reveal_type(v)  # revealed: 1 | 2
```
