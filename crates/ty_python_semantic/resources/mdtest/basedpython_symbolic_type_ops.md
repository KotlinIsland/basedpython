# basedpython: symbolic operations in type expressions

ty already evaluates symbolic operations on literal types in value position — `1 + 1` is inferred as
`Literal[2]`. basedpython lets the same operations appear in a type expression: writing `a: 1 + 1`
declares `a` with the type `Literal[2]`. the evaluation reuses ty's value-level binary-operation
logic, so it works for any operand types ty understands, not just plain `int`s — literals, type
aliases, `typeof`, and combinations thereof.

```toml
[environment]
python-version = "3.13"
```

## the headline: `1 + 1` is `2`

a parameter annotation is an unambiguous type position; the declared type is the folded result

```by
def f(a: 1 + 1) -> None:
    reveal_type(a)  # revealed: 2
```

## type aliases as operands

`type A = 1` makes `A` an alias for `Literal[1]`; `A + B` folds through the aliases

```by
type A = 1
type B = 2

c: A + B = 3
reveal_type(c)  # revealed: 3
```

## `typeof` as an operand

`typeof d` is the static type of `d`; `1 + typeof d` folds the literal into it

```by
let d = 2

def f(e: 1 + typeof d) -> None:
    reveal_type(e)  # revealed: 3
```

## the user's full example

```by
type A = 1
type B = 2

c: A + B = 3
reveal_type(c)  # revealed: 3

let d = 2

e: 1 + typeof d = 3
reveal_type(e)  # revealed: 3
```

## the result is a precise literal type, not `int`

an assignment that does not match the folded literal is rejected

```by
# error: [invalid-assignment] "Object of type `4` is not assignable to `2`"
x: 1 + 1 = 4
```

## a variety of operators

```by
def f(
    sub: 5 - 2,
    mul: 3 * 4,
    pow: 2 ** 8,
    floordiv: 7 // 2,
    mod: 10 % 3,
    lshift: 1 << 4,
    bitxor: 5 ^ 1,
) -> None:
    reveal_type(sub)  # revealed: 3
    reveal_type(mul)  # revealed: 12
    reveal_type(pow)  # revealed: 256
    reveal_type(floordiv)  # revealed: 3
    reveal_type(mod)  # revealed: 1
    reveal_type(lshift)  # revealed: 16
    reveal_type(bitxor)  # revealed: 4
```

## string concatenation

```by
def f(s: "foo" + "bar") -> None:
    reveal_type(s)  # revealed: "foobar"
```

## unary operations

a negative literal is a unary operation; it works on its own and as an operand

```by
def f(neg: -3, expr: -3 * 2, inv: ~0) -> None:
    reveal_type(neg)  # revealed: -3
    reveal_type(expr)  # revealed: -6
    reveal_type(inv)  # revealed: -1
```

## float and complex operands

basedpython admits float and complex literal types, so the same arithmetic folds for them

```by
def f(flt: 1.5 + 1.5, cpx: 1j + 2j) -> None:
    reveal_type(flt)  # revealed: 3.0
    reveal_type(cpx)  # revealed: 3j
```

## composes inside other type forms

a folded symbolic operation is an ordinary type expression — it nests in subscripts and unions. `+`
binds tighter than `|`, so `1 + 1 | 4` is `(1 + 1) | 4`

```by
def f(sub: list[1 + 1], union: 1 + 1 | 4) -> None:
    reveal_type(sub)  # revealed: list[2]
    reveal_type(union)  # revealed: 2 | 4
```

## chained operations

```by
def f(a: 1 + 2 + 3) -> None:
    reveal_type(a)  # revealed: 6
```

## mixed literals and type aliases through `typeof`

```by
type Two = 2

let base = 10

def f(a: Two * typeof base) -> None:
    reveal_type(a)  # revealed: 20
```

## an unsupported operation is still rejected

`+` between two classes is not a meaningful type; it falls through to the standard error

```by
class A: ...
class B: ...

# error: [invalid-type-form]
bad: A + B
```

## value position is unaffected

`|` and `&` keep their dedicated meanings (union / intersection); other operators in value position
are ordinary runtime arithmetic, never folded into a type

```by
x = 1 + 1
reveal_type(x)  # revealed: 2
```
