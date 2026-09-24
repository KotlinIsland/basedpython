# defaults that initialise type parameters

a parameter's default can decide a type parameter: when a call leaves the argument out, the type
parameter is solved from the default, the same as if the default had been passed

```by
def f[T](t: T = 1) -> T:
    return t

one = f()  # `1`
a = f("a")  # `"a"`
```

the default only has to fit *some* specialization of the annotation, not every one. `1` is not a
valid `T` for every `T`, but it is for the `T` that a call leaving `t` out solves

this is purely a checker rule: the program transpiles and runs unchanged

## the default is solved with the other arguments

a default the call falls back on is one more value for the type parameter, alongside the arguments
that were passed

```by
def pair[T](first: T, second: T = 1) -> T:
    return first

either = pair("a")  # `"a" | 1`
```

when an argument pins the type parameter to something the default doesn't fit, the call is an
`invalid-argument-type` error, with a note naming the default that took part

```by
def first[T](items: list[T], fallback: T = 0) -> T:
    return items[0] if items else fallback

def _(names: list[str]):
    name = first(names, "")  # fine
    name = first(names)  # error: `list[T]` makes `T` a `str`, and `0` isn't one
```

## unpacked arguments

an unpacked argument that may be too short to reach the parameter may or may not supply it, so the
default takes part alongside what it unpacks

```by
def f[T](t: T = 1) -> T:
    return t

def _(maybe: tuple[()] | tuple[str]):
    x = f(*maybe)  # `str | 1`: `f(*())` is `f()`
```

## class type parameters

a constructor's defaults initialise the class's type parameters

```by
class Box[T]:
    def __init__(self, item: T = 1):
        self.item = item

    def replace(self, item: T = 1) -> T:
        return item

numbers = Box()  # `Box[int]`
words = Box("a")  # `Box[str]`
```

a method's call is given the class's type parameters by the receiver, so its default is checked
against them

```by
def _(box: Box[str]):
    word = box.replace()  # error: the default `1` is not a `str`

empty = Box[str]()  # error: the default `1` is not a `str`
```

## `some` parameters

a [`some`](sound-types.md) parameter opens an anonymous type parameter, and its default initialises
it like any other. another parameter annotated with that type contributes to it too

```by
def f(n: some int = 1, m: n = 2) -> n:
    return n

n = f(m=5)  # `5 | 1`
```

## inherited defaults

an override that [inherits a default](inherited-defaults.md) solves it against its own annotation,
not the base's

```by
class A:
    def f(self, a: int = 1) -> int:
        return a

class B(A):
    override def f[T](self, a: T) -> T:
        return a

one = B().f()  # `1`
```

## overrides, protocols and overloads keep the default

a caller that holds an `A` and calls `a.m()` solves `T` from the default `A.m` declares, whatever
subclass it really holds. so an override, a protocol implementation, or a function passed where a
callable protocol is expected has to run with a default that the declared one describes

```by
class A:
    def m[T](self, t: T = 1) -> T:
        return t

class B(A):
    override def m[T](self, t: T = "a") -> T:  # error: invalid-method-override
        return t
```

an overload's default makes the same promise about its implementation, which is what runs. an
implementation whose default the overload's doesn't describe is an `invalid-overload` error

## passing the function on

a function whose default initialises a type parameter can stand in for a callable that leaves the
argument out, and a `ParamSpec` that forwards its parameters keeps the default

```by
from collections.abc import Callable

def g[T](t: T = 1) -> T:
    return t

def call[R](f: Callable[[], R]) -> R:
    return f()

one = call(g)  # `1`
```

a `functools.partial` that leaves the parameter to the later call solves the default together with
the arguments it binds, since the later call may fall back on it

## what is still an error

a default no specialization fits is an `invalid-parameter-default` error where it is written. that
includes a default for a type parameter of an enclosing function, which no call can change, and a
default whose own type names the type parameter: a default is typed as the one value made where the
`def` runs, which no single call's type parameter describes

```by
def bounded[T: str](t: T = 1): ...  # error: `1` is never a `str`

def outer[T](value: T):
    def inner(t: T = 1): ...  # error: `T` is fixed by `outer`

def first_or[T](value: T, fallback: list[T] = [1]): ...  # error: `[1]` is a `list[T | int]`
```

an [inherited default](inherited-defaults.md) that fits no specialization of the override's
annotation is reported on the override

the `...` that a stub, an `@overload`, an `@abstractmethod`, a protocol member or a declaration under
`if TYPE_CHECKING:` writes in place of a default is not a value, so it solves nothing

## limitations

assigning such a function to a declared callable type, `c: Callable[[], str] = g`, doesn't take the
default into account yet, so that assignment is accepted
