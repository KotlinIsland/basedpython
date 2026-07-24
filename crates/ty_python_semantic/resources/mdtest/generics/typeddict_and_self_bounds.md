# `TypedDict` and `Self` as type variable bounds

```toml
[environment]
python-version = "3.13"
```

## `TypedDict` as an upper bound

### A type variable can range over every `TypedDict`

The bare `TypedDict` special form is not a type expression anywhere else, but as an upper bound it
denotes the top of the `TypedDict` lattice, so the type variable ranges over every `TypedDict`.

```py
from typing import TypedDict

class Movie(TypedDict):
    name: str

class Book(TypedDict):
    title: str
    pages: int

def f[T: TypedDict](x: T) -> T:
    return x

reveal_type(f(Movie(name="a")))  # revealed: Movie
reveal_type(f(Book(title="b", pages=1)))  # revealed: Book
```

### Non-`TypedDict` arguments are rejected

```py
from typing import TypedDict

def f[T: TypedDict](x: T) -> T:
    return x

# error: [invalid-argument-type] "Argument type `Literal[1]` does not satisfy upper bound `TypedDict` of type variable `T`"
f(1)

# error: [invalid-argument-type] "Argument type `dict[str, str]` does not satisfy upper bound `TypedDict` of type variable `T`"
# error: [invalid-key]
f({"name": "a"})
```

### Generic classes

```py
from typing import TypedDict

class Movie(TypedDict):
    name: str

class Wrapper[T: TypedDict]:
    def __init__(self, value: T) -> None:
        self.value = value

reveal_type(Wrapper(Movie(name="a")).value)  # revealed: Movie

# error: [invalid-type-arguments] "Type `int` is not assignable to upper bound `TypedDict` of type variable `T@Wrapper`"
def _(w: Wrapper[int]) -> None: ...
```

### Legacy type variables

```py
from typing import TypedDict, TypeVar

class Movie(TypedDict):
    name: str

T = TypeVar("T", bound=TypedDict)

def f(x: T) -> T:
    return x

reveal_type(f(Movie(name="a")))  # revealed: Movie
```

### The bound is still invalid in other type expressions

```py
from typing import TypedDict

# error: [invalid-type-form] "The special form `typing.TypedDict` is not allowed in parameter annotations"
def f(x: TypedDict) -> None: ...

# error: [invalid-type-form] "The special form `typing.TypedDict` is not allowed in parameter annotations"
def g[T](x: T, y: TypedDict) -> None: ...
```

### `TypedDict` is not allowed as a constraint

Constraints are an enumeration of concrete types, so the top of the lattice has no meaning there.

```py
from typing import TypedDict, TypeVar

# error: [invalid-type-form] "The special form `typing.TypedDict` is not allowed in type expressions"
T = TypeVar("T", TypedDict, int)
```

### `type[T]` of a `TypedDict`-bounded type variable

```py
from typing import TypedDict

class Movie(TypedDict):
    name: str

def from_class[T: TypedDict](cls: type[T]) -> T:
    raise NotImplementedError

reveal_type(from_class(Movie))  # revealed: Movie
```

### Unpacking a `TypedDict`-bounded type variable

`**kwargs: Unpack[T]` normally expands into keyword-only parameters as soon as the signature is
built. When `T` is a type variable it cannot expand yet, so the parameter stays put and expands once
`T` is solved -- the same deferral basedpython uses for `**kwargs: *Kwargs` packs.

This is the example from [typing#1395]. Note that the typing spec requires a concrete `TypedDict`
here; accepting a type variable is a deliberate extension.

```py
from typing import TypedDict, Unpack

class Thing(TypedDict):
    name: str
    count: int

class A[ExtraArgs: TypedDict]:
    def do_it(self, **extra: Unpack[ExtraArgs]) -> None: ...

a: A[Thing] = A()
a.do_it(name="x", count=1)

# error: [invalid-argument-type] "Expected `str`, found `Literal[1]`"
a.do_it(name=1, count=1)

# error: [missing-argument] "No argument provided for required parameter `count` of bound method `A.do_it`"
a.do_it(name="x")
```

`Thing` is implicitly open, so an undeclared key is accepted, exactly as it would be for a
written-out `Unpack[Thing]`. A closed `TypedDict` rejects it:

```toml
[environment]
python-version = "3.15"
```

```py
from typing import TypedDict, Unpack

class Closed(TypedDict, closed=True):
    name: str

class Box[T: TypedDict]:
    def go(self, **kw: Unpack[T]) -> None: ...

b: Box[Closed] = Box()
b.go(name="x")

# error: [unknown-argument] "Argument `imposter` does not match any known parameter of bound method `Box.go`"
# error: [missing-argument] "No argument provided for required parameter `name` of bound method `Box.go`"
b.go(imposter="y")
```

### A `TypedDict` is solved from the keyword arguments

When nothing else pins the type variable down, it is solved from the keyword arguments as a whole,
rather than each argument being matched against the type variable itself.

```py
from typing import TypedDict, Unpack

def g[T: TypedDict](**kw: Unpack[T]) -> T:
    raise NotImplementedError

reveal_type(g(name="a"))  # revealed: <TypedDict with items 'name'>
```

That only works while the keyword arguments are the *sole* source of the type variable. If another
parameter also mentions it, that parameter decides the type variable before these keywords are
matched, and the deferred parameter could never be re-checked against it -- so the signature is
rejected rather than silently going unchecked.

```py
from typing import TypedDict, Unpack

# error: [invalid-type-form] "Unpacked value for `**kwargs` must be a TypedDict, not `T@f`"
def f[T: TypedDict](proto: T, **kw: Unpack[T]) -> T:
    raise NotImplementedError
```

A type variable that is not bounded by a `TypedDict` is still rejected outright:

```py
from typing import TypedDict, Unpack

# error: [invalid-type-form] "Unpacked value for `**kwargs` must be a TypedDict, not `T@bad`"
def bad[T: int](**kw: Unpack[T]) -> None: ...

# error: [invalid-type-form] "Unpacked value for `**kwargs` must be a TypedDict, not `T@bare`"
def bare[T](**kw: Unpack[T]) -> None: ...
```

### Nested inside a bound

The whole bound is a type-variable-bound context, so `TypedDict` also denotes the top of the lattice
when it appears nested inside one.

```py
from typing import TypedDict

class Movie(TypedDict):
    name: str

def f[T: list[TypedDict]](x: T) -> T:
    return x

# `list` is invariant, so only `list[TypedDict]` itself is below the bound
reveal_type(f([Movie(name="a")]))  # revealed: list[TypedDict]
```

## `Self` as an upper bound

### A method's type variable can be bounded by `Self`

```py
from typing import Self

class Node:
    def link[T: Self](self, other: T) -> T:
        return other

class Sub(Node):
    extra: int

def _(node: Node, sub: Sub) -> None:
    reveal_type(node.link(node))  # revealed: Node
    reveal_type(sub.link(sub))  # revealed: Sub
```

### The bound is enforced

```py
from typing import Self

class Node:
    def link[T: Self](self, other: T) -> T:
        return other

class Unrelated: ...

def _(node: Node) -> None:
    # error: [invalid-argument-type] "Argument type `Unrelated` does not satisfy upper bound `Self@link` of type variable `T`"
    node.link(Unrelated())
```

### `Self` is still rejected outside a class body

```py
from typing import Self

# error: [invalid-type-form] "Variable of type `<special-form 'typing.Self'>` is not allowed in a type expression"
def f[T: Self](x: T) -> T:
    return x
```

## `Self` as a class type parameter default

A class type parameter can default to `Self`, so that a class used bare is generic in the receiver's
own type. This is the `contextlib.AbstractContextManager` pattern: `__enter__` returns the type you
actually called it on, without every subclass having to re-specialize.

### Exact at any depth of subclassing

`Self` stays symbolic until a member is looked up on a receiver, so it is not frozen to the class
that happened to declare the base:

```py
from typing import Self

class Ctx[T = Self]:
    def __enter__(self) -> T:
        raise NotImplementedError
    def __exit__(self, *args: object) -> None: ...

class Sub(Ctx): ...
class SubSub(Sub): ...

def _(s: Sub, ss: SubSub) -> None:
    reveal_type(s.__enter__())  # revealed: Sub
    reveal_type(ss.__enter__())  # revealed: SubSub

with Sub() as x:
    reveal_type(x)  # revealed: Sub

with SubSub() as y:
    reveal_type(y)  # revealed: SubSub
```

### An explicit type argument wins over the default

```py
from typing import Self

class Ctx[T = Self]:
    def enter(self) -> T:
        raise NotImplementedError

class Explicit(Ctx[int]): ...

def _(e: Explicit) -> None:
    reveal_type(e.enter())  # revealed: int
```

### A `Self` default is not an out-of-scope reference

`Self` is bound by the class itself rather than by its type parameter list:

```py
from typing import Self

class Pair[T = Self, U = int]: ...
```

### `Self` cannot bound a class's own type parameter

As a *default*, `Self` stays symbolic and is bound when a member is looked up on a receiver. As a
*bound* there is no such moment: specializing the class (`C[X]`) happens where no receiver exists,
so the bound could never be checked.

```py
from typing import Self

# error: [invalid-type-form] "`Self` cannot bound a type parameter of the class it belongs to"
class C[T: Self]: ...

class Outer:
    # error: [invalid-type-form] "`Self` cannot bound a type parameter of the class it belongs to"
    class Inner[T: Self]: ...
```

A method that simply returns the receiver's own type needs neither, and is unaffected:

```py
from typing import Self

class Node:
    def clone(self) -> Self:
        raise NotImplementedError

class Sub(Node): ...

def _(s: Sub) -> None:
    reveal_type(s.clone())  # revealed: Sub
```

### Other type variables are still rejected as a bound

```py
from typing import TypeVar

S = TypeVar("S")

# error: [invalid-type-variable-bound] "TypeVar upper bound cannot be generic"
def f[T: S](x: T) -> T:
    return x

# error: [invalid-type-variable-bound] "TypeVar upper bound cannot be generic"
def g[U, T: U](x: T) -> T:
    return x
```

[typing#1395]: https://github.com/python/typing/issues/1395
