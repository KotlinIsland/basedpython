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
