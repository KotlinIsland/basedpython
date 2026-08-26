# statement expressions

a compound statement can stand where a value is expected. the value is whatever
its branches evaluate last:

```by
direction = match command:
    case "up":
        1
    case "down":
        -1
    case _:
        0
```

each branch's value is the expression it ends on — no `return`, no assignment
repeated per arm. the type is the union of the branch values, so `direction`
above is `Literal[1, -1, 0]`

the forms that carry a suite are `if`, `match`, `for` and `while`. `raise`,
`return`, `break` and `continue` are also expressions; they never produce a
value, so they type as `Never`

## `if`

```by
label = if count == 0:
    "none"
elif count == 1:
    "one"
else:
    "many"
```

an `if` expression needs an `else`: without one the statement can finish having
evaluated nothing, and there is no value to stand in. that is reported as
`non-exhaustive-statement-expression`

the branches are ordinary suites, so they can do work before their value:

```by
scaled = if noisy:
    log("scaling")
    value * 2
else:
    value
```

## `match`

```by
size = match shape:
    case Circle(r):
        3.14 * r * r
    case Square(s):
        s * s
```

the same exhaustiveness rule applies, and it is the type checker's ordinary
one: the arms have to cover the subject's type. a `case _:` always suffices

## `for` and `while`

a loop yields a value through `break`:

```by
first_even = for n in numbers:
    if n % 2 == 0:
        break n
else:
    -1
```

`break <value>` leaves the loop with that value. the `else` clause — which runs
when the loop finishes without breaking — supplies the value for that path, so
a loop expression without `else` is non-exhaustive

a `break` inside a nested loop belongs to *that* loop, exactly as it does in a
statement

a `break` may only carry a value where something reads it — in a loop that is
not a statement expression there is nowhere for the value to go, and it is
rejected

## the diverging forms

`raise`, `return`, `break` and `continue` are all `Never`, so each can stand in
for a value that will never be produced:

```by
def lookup(table: dict[str, int], key: str) -> int:
    return table.get(key) ?? raise KeyError(key)

def parse_port(raw: str?) -> int:
    text = raw ?? return 0
    return int(text)
```

because they are `Never`, the surrounding expression's type is just the other
branch's — `table.get(key) ?? raise ...` is `int`, not `int | None`

`break` and `continue` bring that to a loop body, where the value that went
missing is a reason to move on rather than to fail:

```by
def total(rows: list[str]) -> int:
    sum = 0
    for row in rows:
        amount = parse(row) ?? continue
        sum += amount
    return sum
```

## nesting

a branch that ends on another compound statement takes *its* value, so the
inner form does not have to be spelled as an expression:

```by
kind = if isinstance(x, str):
    match len(x):
        case 0:
            "empty"
        case _:
            "text"
else:
    "other"
```

## where they can appear

a statement expression owns the tail of the statement it appears in. a form
with a suite must be the whole value of its statement, and must be the first
statement on its line — its suite continues that line, so nothing may precede
it there:

```by
a = match x:           # ok
    case _:
        1

a = 1 + match x:       # error: must be the whole value of its statement
    case _:
        1
```

the diverging forms carry no suite, so they may also appear inside the
operators that *choose* between operands — `and`, `or`, `??`, the conditional
expression, and the walrus:

```by
value = maybe() ?? raise Missing()
value = cached or raise Missing()
value = x if x > 0 else raise ValueError(x)
value = (found := lookup() ?? raise Missing())
value = maybe() ?? continue
```

anywhere else — as a call argument, inside a list, as an operand of `+` — the
surrounding expression would have to be evaluated around the statement, and is
rejected

## lowering

a statement expression lowers to the statement it always was, with the
assignment moved below it:

```by
a = match command:
    case "up":
        1
    case _:
        0
```

```py
match command:
    case "up":
        __by_stmt_expr_0__ = 1
    case _:
        __by_stmt_expr_0__ = 0
a = __by_stmt_expr_0__
```

`break <value>` becomes an assignment followed by a bare `break`. a diverging
statement expression under a choosing operator becomes the guard it implies:

```by
v = table.get(k) ?? raise KeyError(k)
```

```py
__by_stmt_expr_0__ = table.get(k)
if __by_stmt_expr_0__ is None:
    raise KeyError(k)
v = __by_stmt_expr_0__
```
