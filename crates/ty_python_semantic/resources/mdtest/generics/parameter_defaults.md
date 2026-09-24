# parameter defaults that initialise type variables

```toml
[environment]
python-version = "3.13"
```

a parameter's default doesn't have to fit every specialization of the type variables its annotation
names. when it fits only some of them, the default *initialises* those type variables: a call that
leaves the argument out is solved and checked as if the default had been passed in its place.

## solving from the default

`1` is not a valid `T` for every `T`, but it is for some. a call without the argument solves `T`
from the default, exactly as `f(1)` would. a call that passes the argument ignores the default.

```py
def f[T](t: T = 1) -> T:
    return t

reveal_type(f())  # revealed: Literal[1]
reveal_type(f("a"))  # revealed: Literal["a"]
```

the same holds for a keyword-only parameter, and for a legacy type variable, whose constraints the
default picks between.

```py
from typing import TypeVar

def keyword[T](*, t: T = 1) -> T:
    return t

reveal_type(keyword())  # revealed: Literal[1]

IntOrStr = TypeVar("IntOrStr", int, str)

def constrained(t: IntOrStr = 1) -> IntOrStr:
    return t

reveal_type(constrained())  # revealed: int
reveal_type(constrained("a"))  # revealed: str
```

## variadic type parameters

a default initialises a `ParamSpec` or a `TypeVarTuple` the same way.

```py
from collections.abc import Callable

def one(x: int) -> str:
    return ""

def forward[**P](c: Callable[P, str] = one) -> Callable[P, str]:
    return c

reveal_type(forward())  # revealed: (x: int) -> str

def pack[*Ts](t: tuple[*Ts] = (1, "a")) -> tuple[*Ts]:
    return t

reveal_type(pack())  # revealed: tuple[Literal[1], Literal["a"]]
reveal_type(pack((b"x",)))  # revealed: tuple[Literal[b"x"]]
```

## a default no call can use

a default is only accepted when some specialization fits it. a bound or a constraint it violates
rules them all out.

```py
from typing import TypeVar

# error: [invalid-parameter-default] "Default value of type `Literal[1]` is not assignable to annotated parameter type `T@bounded`"
def bounded[T: str](t: T = 1) -> T:
    return t

IntOrStr = TypeVar("IntOrStr", int, str)

# error: [invalid-parameter-default] "Default value of type `Literal[b"x"]` is not assignable to annotated parameter type `IntOrStr@constrained`"
def constrained(t: IntOrStr = b"x") -> IntOrStr:
    return t
```

a type variable of an enclosing function is fixed throughout the body the inner function is defined
in, so no call to the inner function can specialize it to fit.

```py
def outer[T](value: T) -> None:
    # error: [invalid-parameter-default]
    def inner(t: T = 1) -> None: ...
```

## a default whose type names the type variable

a default is typed as one value, made once when the function is defined and shared by every call.
`[1]` inferred against `list[T]` is a `list[T | int]`, a type that names `T`, but no call can make
the list that is actually there hold its own `T`: appending to it in one call would put that call's
`T` into the list the next call sees. so such a default initialises nothing, and is reported.

```py
# error: [invalid-parameter-default] "Default value of type `list[T@first_or | int]` is not assignable to annotated parameter type `list[T@first_or]`"
def first_or[T](value: T, fallback: list[T] = [1]) -> list[T]:
    return fallback
```

a default that fits whatever `T` is, like `[]` against `list[T]`, is unaffected.

```py
def items[T](value: T, into: list[T] = []) -> list[T]:
    return into
```

## defaults that fit every specialization

a default that fits the annotation whatever the type variables are, like `None` in `T | None`, says
nothing about them, so a call that falls back on it is solved from its arguments alone.

```py
def optional[T](t: T | None = None) -> list[T]:
    return []

reveal_type(optional())  # revealed: list[Unknown]
reveal_type(optional(1))  # revealed: list[int]
```

## defaults solved together with arguments

a default that the call falls back on takes part in solving alongside the arguments that were
passed, just as a second argument would.

```py
def pair[T](first: T, second: T = 1) -> T:
    return first

reveal_type(pair("a"))  # revealed: Literal["a", 1]
reveal_type(pair("a", "b"))  # revealed: Literal["a", "b"]

def both[T](first: T = 1, second: T = "a") -> T:
    return first

reveal_type(both())  # revealed: Literal[1, "a"]
reveal_type(both(second=2))  # revealed: Literal[2, 1]
```

when an argument pins the type variable to something the default doesn't fit, the call is rejected.
here the invariant `list[T]` takes `T` to be `str`, which `0` is not, and the diagnostic names the
default that took part.

```py
def first[T](items: list[T], fallback: T = 0) -> T:
    return items[0] if items else fallback

def _(names: list[str]):
    first(names, "")
    first(names)  # snapshot: invalid-argument-type
```

```snapshot
error[invalid-argument-type]: Argument to function `first` is incorrect
  --> src/mdtest_snippet.py:17:11
   |
17 |     first(names)  # snapshot: invalid-argument-type
   |           ^^^^^ Expected `list[str | Literal[0]]`, found `list[str]`
info: element `Literal[0]` of union `str | Literal[0]` is not assignable to `str`
info: no argument is passed for `fallback`, so its default of type `Literal[0]` takes part in solving this call
  --> src/mdtest_snippet.py:12:30
   |
12 | def first[T](items: list[T], fallback: T = 0) -> T:
   |                              ^^^^^^^^^^^^^^^
info: Function defined here
  --> src/mdtest_snippet.py:12:5
   |
12 | def first[T](items: list[T], fallback: T = 0) -> T:
   |     ^^^^^    -------------- Parameter declared here
info: `list` is invariant in its type parameter
info: Consider using the covariant supertype `collections.abc.Sequence`
info: For more information, see https://docs.astral.sh/ty/reference/typing-faq/#invariant-generics
```

## unpacked arguments

an unpacked argument that may be too short to reach the parameter may supply it or leave it out:
`f(*maybe)` is `f()` when `maybe` is empty. so the default takes part alongside the unpacked
elements, and the same goes for the keys of an unpacked mapping and the ones a `TypedDict` doesn't
require.

```py
from typing import TypedDict

def f[T](t: T = 1) -> T:
    return t

class MaybeNamed(TypedDict, total=False):
    t: str

def _(maybe: tuple[()] | tuple[str], by_name: dict[str, str], maybe_named: MaybeNamed):
    reveal_type(f(*maybe))  # revealed: str | Literal[1]
    reveal_type(f(**by_name))  # revealed: str | Literal[1]
    reveal_type(f(**maybe_named))  # revealed: str | Literal[1]
```

a list may be empty too, and it may also be longer than the parameters it unpacks into.

```py
def _(names: list[str]):
    # error: [refutable-unpacking]
    reveal_type(f(*names))  # revealed: str | Literal[1]
```

an unpacked tuple of known length, or a required key of a `TypedDict`, always supplies the
parameter, so the default plays no part.

```py
class Named(TypedDict):
    t: str

def _(pair: tuple[str], named: Named):
    reveal_type(f(*pair))  # revealed: str
    reveal_type(f(**named))  # revealed: str
```

## class type parameters

a constructor's defaults initialise the class's type parameters, whether they are declared on
`__init__` or on `__new__`.

```py
from typing import Self

class Box[T]:
    def __init__(self, item: T = 1) -> None:
        self.item = item

    def replace(self, item: T = 1) -> T:
        return item

reveal_type(Box())  # revealed: Box[int]
reveal_type(Box("a"))  # revealed: Box[str]

class Made[T]:
    def __new__(cls, item: T = 1) -> Self:
        return super().__new__(cls)

reveal_type(Made())  # revealed: Made[Literal[1]]
```

a method is given its class's type parameters by the receiver, so a method's default is checked
against the receiver's specialization. an explicitly specialized `Box[str]` can't be built without
an item either.

```py
reveal_type(Box().replace())  # revealed: int

def _(box: Box[str]):
    box.replace("a")
    box.replace()  # snapshot: invalid-argument-type

# error: [invalid-argument-type] "Default value of parameter `item` of class `Box` does not fit this call: Expected `str`, found `Literal[1]`"
Box[str]()
```

```snapshot
error[invalid-argument-type]: Default value of parameter `item` of bound method `Box.replace` does not fit this call
  --> src/mdtest_snippet.py:22:5
   |
22 |     box.replace()  # snapshot: invalid-argument-type
   |     ^^^^^^^^^^^^^ Expected `str`, found `Literal[1]`
info: Parameter declared here
 --> src/mdtest_snippet.py:7:23
  |
7 |     def replace(self, item: T = 1) -> T:
  |                       ^^^^^^^^^^^
```

class methods and static methods solve their own type parameters from their defaults like any other
function.

```py
class Factory:
    @classmethod
    def make[T](cls, item: T = 1) -> T:
        return item

    @staticmethod
    def build[T](item: T = 1) -> T:
        return item

reveal_type(Factory.make())  # revealed: Literal[1]
reveal_type(Factory().make())  # revealed: Literal[1]
reveal_type(Factory.build())  # revealed: Literal[1]
```

## placeholder defaults

the `...` that a declaration without a body of its own writes in place of a value is not a value. it
neither has to fit the annotation nor solves anything: a call to such a declaration leaves the type
variable unsolved. that covers a stub, an overload, an abstract method, a protocol member, and a
declaration under `if TYPE_CHECKING:`.

```pyi
def stub[T](t: T = ...) -> T: ...
```

```py
from abc import ABC, abstractmethod
from typing import TYPE_CHECKING, Protocol, overload

@overload
def overloaded[T](t: T = ...) -> T: ...
@overload
def overloaded(t: int, u: int) -> int: ...
def overloaded(t=1, u=2):
    return t

reveal_type(overloaded())  # revealed: Unknown

class Abstract(ABC):
    @abstractmethod
    def get[T](self, t: T = ...) -> T: ...

class Gettable(Protocol):
    def get[T](self, t: T = ...) -> T: ...

def _(abstract: Abstract, gettable: Gettable):
    reveal_type(abstract.get())  # revealed: Unknown
    reveal_type(gettable.get())  # revealed: Unknown

if TYPE_CHECKING:
    def declared[T](t: T = ...) -> T: ...

else:
    def declared(t=1):
        return t

reveal_type(declared())  # revealed: Unknown
```

outside of those, `...` is an ordinary value of type `EllipsisType`.

```py
from typing import Self

class Node:
    # error: [invalid-parameter-default] "Default value of type `EllipsisType` is not assignable to annotated parameter type `Self@copy`"
    def copy(self, other: Self = ...) -> Self:
        return self
```

## an overload's default is a promise about the implementation

a call that matches an overload and leaves the argument out is solved from the overload's default,
but it is the implementation that runs, with its own default. so the implementation's default has to
be a value the overload's describes.

```py
from typing import overload

@overload
def get[T](t: T = 1) -> T: ...
@overload
def get(t: int, u: int) -> int: ...
def get(t: object = 1, u: int = 0) -> object:
    return t

reveal_type(get())  # revealed: Literal[1]

@overload
def mismatched[T](t: T = 1) -> T: ...  # snapshot: invalid-overload
@overload
def mismatched(t: int, u: int) -> int: ...
def mismatched(t: object = "a", u: int = 0) -> object:
    return t
```

```snapshot
error[invalid-overload]: Implementation's default for parameter `t` does not fit this overload's default: expected `Literal[1]`, found `Literal["a"]`
  --> src/mdtest_snippet.py:13:5
   |
13 | def mismatched[T](t: T = 1) -> T: ...  # snapshot: invalid-overload
   |     ^^^^^^^^^^    -------- Overload's default declared here
14 | @overload
15 | def mismatched(t: int, u: int) -> int: ...
16 | def mismatched(t: object = "a", u: int = 0) -> object:
   |     ---------- Implementation defined here
```

## a default is part of the type a method declares

a caller that holds an `A` and calls `a.m()` solves `T` from `A.m`'s default, whatever subclass it
really holds. so an override, an implementation of a protocol, or a function passed where a callable
protocol is expected has to fall back on a default that `A.m`'s describes.

```py
from typing import Protocol, override

class A:
    def m[T](self, t: T = 1) -> T:
        return t

class Same(A):
    @override
    def m[T](self, t: T = 1) -> T:
        return t

class Different(A):
    @override
    # error: [invalid-method-override]
    def m[T](self, t: T = "a") -> T:
        return t

def _(a: A):
    reveal_type(a.m())  # revealed: Literal[1]

class Gettable(Protocol):
    def get[T](self, t: T = 1) -> T: ...

class DefaultsToStr:
    def get[T](self, t: T = "a") -> T:
        return t

# error: [invalid-assignment]
gettable: Gettable = DefaultsToStr()

class Call(Protocol):
    def __call__[T](self, t: T = 1) -> T: ...

def defaults_to_str[T](t: T = "a") -> T:
    return t

# error: [invalid-assignment]
call: Call = defaults_to_str
```

## passing a function with such a default

a function whose default initialises a type variable can stand in for a callable that doesn't take
that argument. the type variable is solved from the default.

```py
from collections.abc import Callable

def g[T](t: T = 1) -> T:
    return t

def call[R](f: Callable[[], R]) -> R:
    return f()

reveal_type(call(g))  # revealed: Literal[1]
```

a callable that forwards its parameters with a `ParamSpec` keeps the default, and a call through it
falls back on the default like a direct call does.

```py
def forward[**P, R](f: Callable[P, R]) -> Callable[P, R]:
    return f

reveal_type(forward(g))  # revealed: [T](t: T = 1) -> T
reveal_type(forward(g)())  # revealed: Literal[1]
reveal_type(forward(g)("a"))  # revealed: Literal["a"]
```

a partial application may leave the parameter to the later call, which may fall back on the default,
so the default takes part in solving the partial application too.

```py
from functools import partial

def pair[T](first: T, second: T = 1) -> T:
    return first

reveal_type(partial(pair, "a")())  # revealed: str | int
```

assigning such a function to a declared callable type doesn't yet solve its type variables, so the
default isn't taken into account there.

```py
# TODO: error: [invalid-assignment]
returns_str: Callable[[], str] = g
```

## `some` parameters

`some` opens an anonymous type parameter, which a default initialises too. another parameter
annotated with that type is solved from the default as well as its own argument.

```by
def twice(n: some int = 1) -> n:
    return n

reveal_type(twice())  # revealed: 1
reveal_type(twice(3))  # revealed: 3

def f(n: some int = 1, m: n = 2) -> n:
    return n

reveal_type(f(m=5))  # revealed: 5 | 1
```

## a default that names its own function

a default can name the function it belongs to. solving `T` from it then asks for `T` again, which
stops at a cycle rather than nesting forever.

```py
def g[T](x: T = lambda: g()) -> T:
    return x

result = g()
```
