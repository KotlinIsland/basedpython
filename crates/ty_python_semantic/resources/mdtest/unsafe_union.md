# Unsafe unions

`UnsafeUnion[T1, T2, ..., Tn]` is a gradual type whose materializations are exactly `T1`, `T2`, ...,
`Tn`. It fuses a union with an intersection: a union on the way in, an intersection on the way out.

## Assignment into an unsafe union

### Every materialization is accepted

Anything the plain union accepts is accepted, so every element is assignable to it — as is the union
of the elements:

```py
from ty_extensions import UnsafeUnion

def f(a: UnsafeUnion[int, str]): ...

f(1)
f("s")

def g(x: int | str, y: bool):
    f(x)
    f(y)
```

### Unrelated types are still rejected

Unlike `Any`, the menu of materializations is finite:

```py
from ty_extensions import UnsafeUnion

def f(a: UnsafeUnion[int, str]): ...

# error: [invalid-argument-type]
f(b"bytes")
# error: [invalid-argument-type]
f(None)
```

## Assignment out of an unsafe union

The value is one of its materializations, so it goes wherever any single materialization can go:

```py
from ty_extensions import UnsafeUnion

def takes_int(x: int): ...
def takes_str(x: str): ...
def takes_bytes(x: bytes): ...
def takes_object(x: object): ...
def f(a: UnsafeUnion[int, str]):
    takes_int(a)
    takes_str(a)
    takes_object(a)

    # error: [invalid-argument-type]
    takes_bytes(a)

    b: int = a
    c: str = a
    # error: [invalid-assignment]
    d: bytes = a
```

## Member access

### A member of any materialization resolves

This is the intersection face:

```py
from ty_extensions import UnsafeUnion

def f(a: UnsafeUnion[int, str]):
    reveal_type(a.imag)  # revealed: Literal[0]
    reveal_type(a.upper())  # revealed: str
    reveal_type(a.__class__)  # revealed: UnsafeUnion[type[int], type[str]]
```

### A member no materialization has is an error

```py
from ty_extensions import UnsafeUnion

def f(a: UnsafeUnion[int, str]):
    # error: [unresolved-attribute]
    reveal_type(a.nonexistent)  # revealed: Unknown
```

### The imprecision keeps propagating

When several materializations have the member, the result is itself an unsafe union:

```py
from ty_extensions import UnsafeUnion

class A:
    x: int

class B:
    x: str

def f(a: UnsafeUnion[A, B]):
    reveal_type(a.x)  # revealed: UnsafeUnion[int, str]
```

## Simplification

### A single materialization is not a choice

```py
from ty_extensions import UnsafeUnion, static_assert
from ty_extensions._internal import is_equivalent_to

static_assert(is_equivalent_to(UnsafeUnion[int, int], int))
static_assert(is_equivalent_to(UnsafeUnion[int, UnsafeUnion[str, bytes]], UnsafeUnion[int, str, bytes]))
```

### The order of the menu does not matter

```py
from ty_extensions import UnsafeUnion, static_assert
from ty_extensions._internal import is_equivalent_to

static_assert(is_equivalent_to(UnsafeUnion[int, str], UnsafeUnion[str, int]))
static_assert(not is_equivalent_to(UnsafeUnion[int, str], UnsafeUnion[int, bytes]))
static_assert(not is_equivalent_to(UnsafeUnion[int, str], int | str))
```

### `Any` swallows the menu and `Never` contributes nothing

```py
from typing import Any

from typing_extensions import Never

from ty_extensions import UnsafeUnion, static_assert
from ty_extensions._internal import is_equivalent_to

static_assert(is_equivalent_to(UnsafeUnion[int, Any], Any))
static_assert(is_equivalent_to(UnsafeUnion[int, Never], int))
```

### A nested unsafe union inside a union entry is widened

An entry that is itself a union carrying an unsafe union has a menu of its own, and nothing
flattened it: only an unsafe union at the *top* of an entry was ever unwrapped. So the type grew.
Operator inference distributes over an unsafe union on both operands and unions the results, which
made every operator applied embed the whole previous type in a new entry — doubling its size each
time, and never converging at all inside a loop, where each fixpoint round adds another level.

The entry is widened to its top materialization instead. Everything the entry admitted is still
assignable to it; what is given up is the intersection face of that one arm.

```py
from ty_extensions import UnsafeUnion, static_assert
from ty_extensions._internal import is_equivalent_to

static_assert(
    is_equivalent_to(UnsafeUnion[int, str | UnsafeUnion[bytes, memoryview]], UnsafeUnion[int, str | bytes | memoryview])
)
```

### Repeated operators do not grow the menu

The regression this protects against: `t + s + s + …` where both operands are unsafe unions used to
double in size per operator. The menu is the same however many are applied.

```py
from ty_extensions import UnsafeUnion

def f(t: UnsafeUnion[int, str], s: UnsafeUnion[int, str]):
    one = t + s
    four = t + s + s + s + s
    reveal_type(one)  # revealed: UnsafeUnion[int, str]
    reveal_type(four)  # revealed: UnsafeUnion[int, str]
```

## Gradual properties

### Subtyping and assignability

An unsafe union is not a fully static type: it is a subtype only of `object`.

```py
from ty_extensions import UnsafeUnion, static_assert
from ty_extensions._internal import is_subtype_of, is_assignable_to

static_assert(not is_subtype_of(UnsafeUnion[int, str], int))
static_assert(not is_subtype_of(int, UnsafeUnion[int, str]))
static_assert(is_subtype_of(UnsafeUnion[int, str], object))

static_assert(is_assignable_to(UnsafeUnion[int, str], int))
static_assert(is_assignable_to(int, UnsafeUnion[int, str]))
static_assert(not is_assignable_to(UnsafeUnion[int, str], bytes))
static_assert(not is_assignable_to(bytes, UnsafeUnion[int, str]))
```

### Materializations

The top and bottom materializations are the union and the intersection of the elements:

```py
from ty_extensions import Bottom, Intersection, Top, UnsafeUnion, static_assert
from ty_extensions._internal import is_equivalent_to

static_assert(is_equivalent_to(Top[UnsafeUnion[int, str]], int | str))
static_assert(is_equivalent_to(Bottom[UnsafeUnion[int, str]], Intersection[int, str]))
```

### Disjointness

It overlaps whatever any of its materializations overlaps, so it is disjoint from a type only when
every materialization is:

```py
from ty_extensions import UnsafeUnion, static_assert
from ty_extensions._internal import is_disjoint_from

static_assert(not is_disjoint_from(UnsafeUnion[int, str], int))
static_assert(is_disjoint_from(UnsafeUnion[int, str], None))
```

## Operators

An operator resolves through the materializations that support it:

```py
from ty_extensions import UnsafeUnion

def f(a: UnsafeUnion[int, str]):
    reveal_type(a + 1)  # revealed: int
    reveal_type(-a)  # revealed: int
    reveal_type(a[0])  # revealed: str
```

## Ambiguous overload calls

### A gradual argument infers the menu of possible returns

Rather than discarding the information as `Unknown`:

```py
from typing import Any, overload

@overload
def f(a: int) -> int: ...
@overload
def f(a: str) -> str: ...
def f(a: int | str) -> int | str:
    return a

def _(a: Any):
    reveal_type(f(a))  # revealed: UnsafeUnion[int, str]
    reveal_type(f(a).imag)  # revealed: Literal[0]
    reveal_type(f(a).upper())  # revealed: str
```

### An overloaded `__new__` returning something else

A `__new__` that can return something other than an instance of the class carries the same
ambiguity, and used to degrade to `Unknown`:

```py
from typing import Any, overload

class C:
    @overload
    def __new__(cls, a: int) -> "C": ...
    @overload
    def __new__(cls, a: str) -> int: ...
    def __new__(cls, a: int | str) -> "C | int":
        return 1

def _(a: Any):
    reveal_type(C(a))  # revealed: UnsafeUnion[int, C]
```

### An overloaded `__new__` never returning an instance

```py
from typing import Any, overload

class E:
    @overload
    def __new__(cls, a: int) -> int: ...
    @overload
    def __new__(cls, a: str) -> str: ...
    def __new__(cls, a: int | str) -> "int | str":
        return 1

def _(a: Any):
    reveal_type(E(a))  # revealed: UnsafeUnion[int, str]
```

### Instance returns still collapse to the class

When every matching overload returns an instance of the constructed class, that class is still the
answer:

```py
from typing import Any, overload

class C:
    @overload
    def __new__(cls, a: int) -> "C": ...
    @overload
    def __new__(cls, a: str) -> "D": ...
    def __new__(cls, a: int | str) -> "C":
        return super().__new__(cls)

class D(C): ...

def _(a: Any):
    reveal_type(C(a))  # revealed: C
```

### Agreeing return types are not a choice

```py
from typing import Any, overload

@overload
def f(a: int) -> bytes: ...
@overload
def f(a: str) -> bytes: ...
def f(a: int | str) -> bytes:
    return b""

def _(a: Any):
    reveal_type(f(a))  # revealed: bytes
```

## Invalid forms

```py
from ty_extensions import UnsafeUnion

# error: [invalid-type-form]
def f(a: UnsafeUnion): ...
```
