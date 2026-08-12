# basedpython: deferred type-level operations

basedpython folds operations on literals at the type level — `Array[1 + 1]` is `Array[2]`, `1 < 2`
is `Literal[True]`, `"ab".startswith("a")` is `Literal[True]`. when an operand of such an operation
is a type parameter, the operation cannot be evaluated at definition time. instead of collapsing it
to the parameter's bound (which would lose the relationship), the operation is kept symbolic and
re-evaluated when the parameter is specialized at a call site.

## arithmetic on a type parameter

```by
class Array[Dim: int]

def extend[Dim: int](a: Array[Dim]) -> Array[Dim + 1]:
    return a

def foo(data: Array[5]):
    data2 = extend(data)
    reveal_type(data2)  # revealed: Array[6]
```

## a plain value type parameter

```by
def succ[I: int](i: I) -> I + 1:
    return i + 1

reveal_type(succ(4))  # revealed: 5
reveal_type(succ(41))  # revealed: 42
```

## the operation is re-applied each time it is specialized

```by
class Array[Dim: int]:
    pass

def extend[Dim: int](a: Array[Dim]) -> Array[Dim + 1]:
    return a

def shrink[Dim: int](a: Array[Dim]) -> Array[Dim - 2]:
    return a

def foo(data: Array[5]):
    reveal_type(extend(extend(data)))  # revealed: Array[7]
    reveal_type(shrink(data))  # revealed: Array[3]
```

## nested arithmetic

```by
def f[I: int](i: I) -> I * 2 + 1:
    return i * 2 + 1

reveal_type(f(10))  # revealed: 21

def g[I: int](i: I) -> -I + 1:
    return -i + 1

reveal_type(g(5))  # revealed: -4
```

## unary operations

```by
def neg[I: int](i: I) -> -I:
    return -i

def inv[I: int](i: I) -> ~I:
    return ~i

reveal_type(neg(5))  # revealed: -5
reveal_type(inv(0))  # revealed: -1
```

## comparisons

```by
def lt[I: int](i: I) -> I < 10:
    return i < 10

def eq[I: int](i: I) -> I == 5:
    return i == 5

reveal_type(lt(3))  # revealed: True
reveal_type(lt(20))  # revealed: False
reveal_type(eq(5))  # revealed: True
reveal_type(eq(6))  # revealed: False
```

## only a single rich comparison folds

a chain and a membership test have no fold, so they are rejected — and the message has to say which
shape *does* fold, since the arm above proves comparisons are allowed here in general.

```by
class Holder[I: int]:
    # error: [invalid-type-form] "A chained comparison has no symbolic fold"
    def chained(self) -> 0 < I < 10:
        return True

    # error: [invalid-type-form] "An identity or membership comparison has no symbolic fold"
    def member(self) -> I in (1, 2, 3):
        return True
```

## method calls on a type parameter

a method call in a type expression views a bare type parameter as an instance of its bound (the same
view a method body has), and re-folds known literal methods once the parameter is a literal

```by
def starts[S: str](s: S) -> S.startswith("foo"):
    return s.startswith("foo")

reveal_type(starts("foobar"))  # revealed: True
reveal_type(starts("bar"))  # revealed: False
```

## a fully concrete operation folds immediately

operations whose operands are already concrete are folded at definition time, in type position just
as on values

```by
def lit() -> "ab".startswith("a"):
    return "ab".startswith("a")

reveal_type(lit())  # revealed: True

class Array[Dim: int]:
    pass

def two() -> Array[1 + 1]:
    return Array[2]()

reveal_type(two())  # revealed: Array[2]
```

## specializing with a non-literal keeps the reduced form

when a type parameter is specialized to a non-literal (here `int`), the deferred operation reduces
to the ordinary operation result

```by
class Array[Dim: int]:
    pass

def extend[Dim: int](a: Array[Dim]) -> Array[Dim + 1]:
    return a

def foo(data: Array[int]):
    reveal_type(extend(data))  # revealed: Array[int]
```

## a body is checked against the operation, not its reduced form

`I + 1` names one value per specialization, so a body has to produce *that* value. checking against
the reduced form would ask only for an `int`, which `i` already is.

```by
def wrong[I: int](i: I) -> I + 1:
    # error: [invalid-return-type] "Return type does not match returned value: expected `I@wrong + 1`, found `I@wrong`"
    return i

def off_by_one[I: int](i: I) -> I + 1:
    # error: [invalid-return-type] "Return type does not match returned value: expected `I@off_by_one + 1`, found `I@off_by_one + 2`"
    return i + 2

def unrelated[I: int](i: I, n: int) -> I + 1:
    # error: [invalid-return-type] "Return type does not match returned value: expected `I@unrelated + 1`, found `int`"
    return n
```

the arithmetic on values is kept symbolic for this, so the two sides can be compared at all:

```by
def succ[I: int](i: I) -> I + 1:
    reveal_type(i + 1)  # revealed: I@succ + 1
    return i + 1
```

## agreement is decided by value, not by shape

two expressions naming the same value need not be written the same way. operands may be commuted,
constants folded together, and terms cancelled.

```by
def commuted[I: int](i: I) -> I + 1:
    return 1 + i

def cancelling[I: int](i: I) -> I + 1:
    return (i + 3) - 2

def rearranged[I: int](i: I) -> I * 2 + 1:
    return 1 + 2 * i

def scaled[I: int](i: I) -> I * 2:
    return i + i
```

a bare type parameter is the expression `I`, so the agreement is decided the same way when only the
*body* is arithmetic — the terms cancelling back to what was asked for is an agreement like any
other:

```by
def cancels[I: int](i: I) -> I:
    return i + 1 - 1

def added_nothing[I: int](i: I) -> I:
    return i + 0

def scaled_by_one[I: int](i: I) -> I:
    return i * 1

def twice_negated[I: int](i: I) -> I:
    return -(-i)

def off_by_one[I: int](i: I) -> I:
    # error: [invalid-return-type] "Return type does not match returned value: expected `I@off_by_one`, found `I@off_by_one + 1`"
    return i + 1
```

which is the same relation the other way round, where the annotation carries the terms that cancel:

```by
def annotated[I: int](i: I) -> I + 0:
    return i
```

that holds of several parameters at once — the terms are compared as terms, not in the order the
expression happens to introduce them:

```by
def sum_of[A: int, B: int](a: A, b: B) -> A + B:
    return b + a

def difference[A: int, B: int](a: A, b: B) -> A - B:
    # error: [invalid-return-type] "Return type does not match returned value: expected `A@difference - B@difference`, found `B@difference - A@difference`"
    return b - a
```

a call whose own return type is symbolic composes, so applying `succ` twice reaches `I + 2`:

```by
def succ[I: int](i: I) -> I + 1:
    return i + 1

def twice[I: int](i: I) -> I + 2:
    return succ(succ(i))

def thrice[I: int](i: I) -> I + 3:
    # error: [invalid-return-type] "Return type does not match returned value: expected `I@thrice + 3`, found `I@thrice + 1 + 1`"
    return succ(succ(i))
```

## subtraction and the unary operators

```by
def pred[I: int](i: I) -> I - 1:
    return i - 1

def negate[I: int](i: I) -> -I:
    return -i

def wrong_sign[I: int](i: I) -> -I:
    # error: [invalid-return-type] "Return type does not match returned value: expected `-I@wrong_sign`, found `I@wrong_sign`"
    return i

def invert[I: int](i: I) -> ~I:
    return ~i
```

`~I` is left whole rather than rewritten as `-I - 1`, so it agrees with itself and nothing else:

```by
def spelled_out[I: int](i: I) -> ~I:
    # error: [invalid-return-type] "Return type does not match returned value: expected `~I@spelled_out`, found `-I@spelled_out - 1`"
    return -i - 1
```

## a union arm is met by the arm, not by the reduced type

a symbolic value has to meet a union target arm by arm. reducing it to `int` first would ask the
question of a value nobody wrote, and `(I + 1) | None` would then reject `i + 1`.

```by
def deferred_arm[I: int](i: I) -> (I + 1) | None:
    return i + 1

def other_arm[I: int](i: I) -> (I + 1) | None:
    return None

def cancelling_arm[I: int](i: I) -> I | None:
    return i + 1 - 1

def annotated_arm[I: int](i: I) -> (I + 0) | None:
    return i

def not_that_value[I: int](i: I) -> (I + 1) | None:
    # error: [invalid-return-type] "Return type does not match returned value: expected `I@not_that_value + 1 | None`, found `I@not_that_value`"
    return i
```

## a gradual value still satisfies a symbolic return type

`Unknown` stands for anything, including the value the annotation names — rejecting it would make an
unannotated helper unusable in symbolic code.

```by
from ty_extensions import Unknown

def unannotated(x) -> Unknown:
    return x

def gradual[I: int](i: I) -> I + 1:
    return unannotated(i)
```

## a method call agrees only with the same call

`+`, `-`, `*` and the unary operators flatten to a form that decides whether two expressions name
the same value. a method call has no such form, but it does not need one: it stands for itself, so
the only body that names its value is the one that makes the same call.

```by
def starts[S: str](s: S) -> S.startswith("foo"):
    return s.startswith("foo")

reveal_type(starts("foobar"))  # revealed: True
reveal_type(starts("bar"))  # revealed: False

def wrong[S: str](s: S) -> S.startswith("foo"):
    # error: [invalid-return-type]
    return False

def other_prefix[S: str](s: S) -> S.startswith("foo"):
    # error: [invalid-return-type]
    return s.startswith("bar")
```

## a call on a `some` parameter

a `some` annotation opens an ordinary type parameter, so a call on it is checked the same way.

```by
def starts(s: some str) -> s.startswith("foo"):
    return s.startswith("foo")

reveal_type(starts("foobar"))  # revealed: True

def wrong(s: some str) -> s.startswith("foo"):
    # error: [invalid-return-type]
    return True
```

## a comparison and an attribute type are checked only against their reduced form

neither has a decision procedure: a comparison has no value-level counterpart to compare against,
and an attribute type is *defined* to read as the bound's member before specialization. a body
annotated with either is checked only against the reduced type.

```by
class A:
    a: int

def compares[I: int](i: I) -> I < 10:
    return True

def member[T: A](t: T) -> T.a:
    return 1
```

## a product of two parameters is not linear

neither operand is a constant, so the product stands for itself and agrees only with an identical
expression.

```by
def area[W: int, H: int](w: W, h: H) -> W * H:
    return w * h

def transposed[W: int, H: int](w: W, h: H) -> W * H:
    # error: [invalid-return-type] "Return type does not match returned value: expected `W@transposed * H@transposed`, found `H@transposed * W@transposed`"
    return h * w
```

## an unprovable body can be cast

a body may be correct for a reason the checker cannot see. the escape hatch is the one every other
unprovable assignment uses.

```by
def from_len[I: int](i: I, xs: list[int]) -> I + 1:
    return len(xs) cast I + 1
```

## an operation nested past the limit stands for its reduced form

keeping an operation symbolic is worth it for a relationship somebody wrote down, and those are one
or two operations deep. past a fixed depth the operation reads as the type it already reads as
everywhere but under type-mapping, so the chain stops growing.

```by
def near[I: int](i: I) -> I + 1 + 1 + 1:
    return i + 1 + 1 + 1

reveal_type(near(10))  # revealed: 13

def past[I: int](i: I) -> int:
    return i + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1

reveal_type(past(10))  # revealed: int
```

## arithmetic accumulated by a loop terminates

a loop that adds to a value typed by an unannotated parameter's hole builds one more operation every
time round the fixpoint. the depth limit is what gives that a fixed point instead of running the
checker out of cycle iterations.

```by
def count_up(start=1):
    i = start
    while True:
        reveal_type(i)  # revealed: int
        i += 1
```
