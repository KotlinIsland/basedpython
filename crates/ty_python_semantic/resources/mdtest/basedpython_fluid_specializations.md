# basedpython: fluid specializations

a binding like `a = [1]` or `a = A(1)` creates a generic instance whose specialization was inferred
rather than declared. while no other observer of the value exists, later uses of the binding may
refine ("widen") the inferred specialization instead of being checked against it. once the value
escapes to an observer that relies on the specialization, it is "locked": the escape's declared type
is adopted if there is one, and later incompatible uses are errors again

element types are promoted (`a = [1]` is `list[int]`, not `list[Literal[1]]`). retaining literals is
the intended behavior, but literal-parametrized generics are currently too expensive in the
cross-module constraint solver, so precision is traded for performance until that is addressed (see
the `TODO(perf)` notes in `fluid.rs` and the collection-literal inference)

this is a basedpython enhancement that also applies to plain python files

```toml
[environment]
python-version = "3.12"
```

## collection literals

the creation-time specialization uses the promoted element type. reads use the narrowed type,
widening uses are not errors — the promoted element type accumulates in the specialization at later
uses

```py
a = [1]
reveal_type(a)  # revealed: list[int]
reveal_type(a[0])  # revealed: int

a.append(2)
reveal_type(a)  # revealed: list[int]
reveal_type(a[0])  # revealed: int

a.append("a")
reveal_type(a)  # revealed: list[int | str]
```

## empty collections

an empty collection literal has no elements, so its creation-time specialization is `Never` — the
precise element type of an empty collection — rather than the gradual `Unknown`. later uses widen it
from there

```py
a = []
reveal_type(a)  # revealed: list[Never]

a.append(1)
reveal_type(a)  # revealed: list[int]

d = {}
reveal_type(d)  # revealed: dict[Never, Never]
```

the `Never` element type is flow-sensitive: the public type observed by uses that are not tracked
(e.g. a binding assigned in multiple branches) stays gradual, so widening through those uses is not
an error

```py
def _(flag: bool):
    if flag:
        a = []
    else:
        a = []

    # `a` has two bindings, so this use is not flow-sensitively tracked; the public type is gradual
    a.append(1)
    reveal_type(a)  # revealed: list[Unknown]
```

## constructor calls

```py
class A[T]:
    def __init__(self, t: T):
        self.t = t

    def x(self) -> T:
        return self.t

    def y(self, t: T): ...

def foo(a: A[object]): ...

a = A(1)
reveal_type(a)  # revealed: A[int]
reveal_type(a.x())  # revealed: int

# an invariant observer locks the specialization to its declared type
foo(a)
reveal_type(a)  # revealed: A[object]
reveal_type(a.x())  # revealed: object
```

## contravariant method use widens

```py
class A[T]:
    def __init__(self, t: T):
        self.t = t

    def x(self) -> T:
        return self.t

    def y(self, t: T): ...

a = A(1)
reveal_type(a.x())  # revealed: int

a.y(object())
reveal_type(a.x())  # revealed: object
```

## reads don't lock

covariant operations (subscript loads, truthiness tests, iteration, bare reads, read-only method
calls) neither widen nor lock the specialization

```py
a = [1]
a[0]
print(a)
len(a)

if a:
    pass

for x in a:
    reveal_type(x)  # revealed: int

a
a.pop()
reveal_type(a)  # revealed: list[int]

a.append("a")
reveal_type(a)  # revealed: list[int | str]
```

## aliasing locks

binding the value to another name creates an observer the checker no longer tracks: the
specialization is promoted and locked, and later widening uses are errors again

```py
a = [1]
a.append(2)
reveal_type(a)  # revealed: list[int]

b = a
reveal_type(b)  # revealed: list[int]
reveal_type(a)  # revealed: list[int]
reveal_type(a[0])  # revealed: int
reveal_type(b[0])  # revealed: int

a.append("x")  # error: [invalid-argument-type]
reveal_type(a)  # revealed: list[int]
```

## adopting a declared specialization locks

passing the value to a context whose declared type constrains the class typevars adopts that
specialization and locks the binding

```py
def wants_ints(v: list[int]): ...

a = [1]
wants_ints(a)
a.append("x")  # error: [invalid-argument-type]
reveal_type(a)  # revealed: list[int]
```

the same applies to annotated assignments:

```py
a = [1]
b: list[int] = a
a.append("x")  # error: [invalid-argument-type]
reveal_type(a)  # revealed: list[int]
```

and to returns:

```py
def f() -> list[int]:
    a = [1]
    return a
```

## widening before a lock is kept

```py
def wants_objects(v: list[object]): ...

a = [1]
a.append("x")
reveal_type(a)  # revealed: list[int | str]

wants_objects(a)
reveal_type(a)  # revealed: list[object]
```

## subscript stores widen

```py
a = [1]
a[0] = "x"
reveal_type(a)  # revealed: list[int | str]
```

## widening in branches

a widening that may have executed is visible at later uses

```py
def coin() -> bool:
    return True

a = [1]

if coin():
    a.append("x")

reveal_type(a)  # revealed: list[int | str]
```

## widening in loops

inside a loop, a widening event is visible at every use in the loop, including uses that appear
earlier in the source. a loop event may execute any number of times with different values, so its
literal types are promoted

```py
def coin() -> bool:
    return True

a = [1]

while coin():
    reveal_type(a)  # revealed: list[int | str]
    a.append("x")

reveal_type(a)  # revealed: list[int | str]
```

this keeps self-feeding loops convergent:

```py
def coin() -> bool:
    return True

nums = [1]

while coin():
    nums.append(nums[0] + 1)
```

## only inferred specializations are fluid

an explicit specialization is a declaration, not an inference — it is checked as usual

```py
a = list[int]([1])
a.append("x")  # error: [invalid-argument-type]
reveal_type(a)  # revealed: list[int]
```

so is an annotated binding:

```py
a: list[int] = [1]
a.append("x")  # error: [invalid-argument-type]
reveal_type(a)  # revealed: list[int]
```

## values from other functions are not fluid

a value returned by a function call already has another observer (the callee), so its specialization
is never fluid

```py
def make() -> list[int]:
    return [1]

a = make()
a.append("x")  # error: [invalid-argument-type]
reveal_type(a)  # revealed: list[int]
```

## dict and set literals

```py
s = {1}
reveal_type(s)  # revealed: set[int]
s.add("x")
reveal_type(s)  # revealed: set[int | str]

d = {1: "a"}
reveal_type(d)  # revealed: dict[int, str]
d[2.0] = b"y"
reveal_type(d)  # revealed: dict[int | float, str | bytes]
```

## generic parameters don't lock

a method (or a generic function parameter) can never share a perspective on the specialization — it
adapts to whatever the caller provides. only a concrete declared type can lock one in

```py
def f1[T](a: list[T]): ...
def f2(a: list[int | str]): ...

a = [1]
f1(a)  # a parametric observer — doesn't lock
reveal_type(a[0])  # revealed: int

a.append(2)
a.append(3)
reveal_type(a)  # revealed: list[int]

f2(a)  # a concrete observer — locks the specialization in
reveal_type(a[0])  # revealed: int | str
reveal_type(a)  # revealed: list[int | str]
```

a call whose return type mentions the typevars solved from the argument hands the caller a new
observer of the specialization (the result aliases the value), so it locks — to exactly the view the
observer received:

```py
def f[T](t: list[T]) -> list[T]:
    return t

a = [1]
xi = f(a)
reveal_type(xi)  # revealed: list[int]
reveal_type(a)  # revealed: list[int]

a.append("x")  # error: [invalid-argument-type]
```

a covariant observer never consumes from the value, so its perspective stays valid under any future
widening: the binding stays fluid, and the observer's type is solved against the binding's eventual
specialization

```py
from typing import Sequence

def f[T](t: list[T]) -> Sequence[T]:
    return t

a = [1]
b = f(a)
reveal_type(b)  # revealed: Sequence[int]

a.append(2)
a.append(3)
reveal_type(a)  # revealed: list[int]

c = a
reveal_type(c)  # revealed: list[int]
reveal_type(a)  # revealed: list[int]
```

the covariant observer's view accounts for widenings that happen after the call:

```py
from typing import Sequence

def f[T](t: list[T]) -> Sequence[T]:
    return t

x = [1]
y = f(x)
reveal_type(y)  # revealed: Sequence[int | str]

x.append("s")
reveal_type(x)  # revealed: list[int | str]
```

but only when the result is actually captured — a discarded result is an observer that does not
survive the call:

```py
def f[T](t: list[T]) -> list[T]:
    return t

a = [1]
f(a)
reveal_type(a)  # revealed: list[int]

a.append(2)
reveal_type(a)  # revealed: list[int]
```

the same holds for a bare-typevar identity, which observes the narrow view:

```py
def ident[T](x: T) -> T:
    return x

d = [1]
yi = ident(d)
reveal_type(yi)  # revealed: list[int]
reveal_type(d)  # revealed: list[int]
```

but a generic function whose return type does not mention the solved typevars creates no surviving
observer, and the binding stays fluid:

```py
def g[T](t: list[T]) -> int:
    return len(t)

b = [1]
g(b)
reveal_type(b)  # revealed: list[int]

b.append(2)
reveal_type(b)  # revealed: list[int]
```

## typevar-blind observers don't lock

a context whose declared type places no requirements on the class typevars (e.g. `object`, `Sized`)
cannot observe the specialization, so the binding stays fluid

```py
def blind(v: object): ...

a = [1]
print(a)
blind(a)
reveal_type(a)  # revealed: list[int]

a.append("x")
reveal_type(a)  # revealed: list[int | str]
```

## disabling fluid specializations

the feature can be turned off with `analysis.disable-fluid-specializations`. each binding then keeps
its creation-time specialization and is never widened or locked by later uses, so an incompatible
later use is an error again

```toml
[environment]
python-version = "3.12"

[analysis]
disable-fluid-specializations = true
```

```py
class A[T]:
    def __init__(self, t: T):
        self.t = t

    def x(self) -> T:
        return self.t

# collection-literal widening is off: the specialization is fixed at creation
a = [1]
a.append("x")  # error: [invalid-argument-type]
reveal_type(a)  # revealed: list[int]

# subscript stores no longer widen
b = [1]
b[0] = "x"  # error: [invalid-assignment]
reveal_type(b)  # revealed: list[int]

# constructor calls keep their creation-time specialization
c = A(1)
reveal_type(c)  # revealed: A[int]
reveal_type(c.x())  # revealed: int
```
