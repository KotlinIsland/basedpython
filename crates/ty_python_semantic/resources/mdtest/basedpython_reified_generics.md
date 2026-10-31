# basedpython: reified type parameters

A PEP 695 type parameter is *reified* when it is declared `reified`, or when the function body
references it in a value position — anywhere other than a type annotation. The reference becomes a
real runtime value (the supplied type argument), so it types as `TypeForm[T]` rather than as the
`TypeVar` object. Reification makes the `[...]` specialization step required — written explicitly,
or inferred from the arguments and injected by the transpiler.

A class reifies too, from the same rule but through its instances; that is
`basedpython_reified_class_generics.md`. This file is about functions.

## a type parameter read as a value is a type form

a type argument can be any type expression, not only a class: `1`, `None` and `int | str` are all
type arguments, and the value is the object that was written. so the value of `T` is a `TypeForm`

```by
def f[T]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]

f[int]()
f[1]()
f[None]()
f[int | str]()
f[list[int]]()
```

a type form isn't callable, so an unbounded `T` can't be called, in a function or in a class

```by
def f[T]() -> None:
    T()  # error: [call-non-callable] "Object of type `TypeForm[T]` is not callable"

class A[T]:
    def m(self) -> None:
        T()  # error: [call-non-callable] "Object of type `TypeForm[T]` is not callable"
```

a default only fills in a type argument that wasn't given, so `f[1]` is still a specialization of
this `f`, and `object` promises no more than leaving the bound off does

```by
def defaulted[T = int]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def top[T: object]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]
```

## a type form reads as a class once it is known to be one

`isinstance(T, type)` narrows it, as it does any `TypeForm`

```by
def build[T]() -> object:
    if isinstance(T, type):
        reveal_type(T)  # revealed: type[T]
        return T()
    return None

assert build[int]() == 0
assert build[None]() is None
assert build[int | str]() is None
assert build[list[int]]() is None

def guarded[T]() -> object:
    if not isinstance(T, type):
        return None
    reveal_type(T)  # revealed: type[T]
    return T()

assert guarded[int]() == 0
assert guarded[None]() is None
```

a literal the checker solved is a type argument and not a class, so it takes the other branch

```by
def solved[T](t: T) -> object:
    if isinstance(T, type):
        return T()
    return None

assert solved(1) is None
assert solved("a") is None
assert solved(int(1)) == 0
```

a class narrows the same way

```by
class Default[T]:
    def make(self) -> object:
        if isinstance(T, type):
            reveal_type(T)  # revealed: type[T]
            return T()
        return None

assert Default[int]().make() == 0
assert Default[None]().make() is None
```

and a union of subclasses, which satisfies a class bound, isn't a class

```by
class Base: ...

class A(Base): ...

class B(Base): ...

def bounded[T: Base]() -> object:
    if isinstance(T, type):
        return T()
    return None

assert bounded[A | B]() is None
assert isinstance(bounded[A](), A)
```

## a narrowed call is checked against the bound

```by
class Needs:
    def __init__(self, x: int) -> None: ...

def needs[T: Needs]() -> None:
    if isinstance(T, type):
        reveal_type(T)  # revealed: type[T]
        T()  # error: [missing-argument]
        T(1)  # no diagnostic
```

## the branch where it isn't a class stays a type form

```by
def otherwise[T]() -> None:
    if isinstance(T, type):
        T()  # no diagnostic
    else:
        reveal_type(T)  # revealed: TypeForm[T] & not type
        T()  # error: [call-non-callable]
```

## a bound doesn't make the value a class

a union of subclasses satisfies a class bound, so `g[A | B]` is a specialization of this `g` and its
value isn't a class

```by
class Base: ...

class A(Base): ...

class B(Base): ...

def g[T: Base]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]
```

so every bound leaves a type form, whatever it names

```by
from typing import Any, Literal, NewType, Protocol, TypedDict
from ty_extensions import Intersection

class Other: ...

class Proto(Protocol): ...

class Fields(TypedDict): ...

Id = NewType("Id", int)

type Alias = Base
type Nothing = None

def named[T: int]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def mapping[T in (int, str)]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def mapping_none[T in (int, None)]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def aliased[T: Alias]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def aliased_none[T: Nothing]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def chained[B: Base, T: B]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def chained_free[U, T: U]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def both[T: Intersection[Base, Other]]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def protocol[T: Proto]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def typed_dict[T: Fields]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def new_type[T: Id]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def dynamic[T: Any]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def pair[T: (int, str)]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def union[T: int | str]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def none[T: None]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def literal[T: Literal[1]]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def callable_bound[T: () -> int]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]

def class_bound[T: type[int]]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    T()  # error: [call-non-callable]
```

a bound that was already reported as unresolved says nothing about the value, so a read of it isn't
reported a second time

```by
# error: [unresolved-reference]
def missing[T: Missing]() -> None:
    T()  # no diagnostic
```

## what a class has to offer isn't on a type form

each of these fails at runtime on the same type arguments that calling `T` does. `x: type[T] = T` is
what would otherwise hand the same object to something that calls it

```by
def reads[T](x: object) -> None:
    T.__name__  # error: [unresolved-attribute]
    isinstance(x, T)  # error: [invalid-argument-type]
    y: type[T] = T  # error: [invalid-assignment]

def bounded[T: int](x: object) -> None:
    T.__name__  # error: [unresolved-attribute]
    isinstance(x, T)  # error: [invalid-argument-type]
    y: type[T] = T  # error: [invalid-assignment]

def narrowed[T: int](x: object) -> None:
    if isinstance(T, type):
        T.__name__  # no diagnostic
        isinstance(x, T)  # no diagnostic
        y: type[T] = T  # no diagnostic

def construct[U](t: type[U]) -> U:
    return t()

def forwarded[T]() -> None:
    construct(T)  # error: [invalid-argument-type]

def stored[T]() -> None:
    held = T
    held()  # error: [call-non-callable]
```

## what a type form can be used for

printing it, comparing it, testing against it and handing it on all work for every type argument

```by
class Box[T]:
    init(var t: T)

def g[T]() -> object:
    return T

def uses[T](t: T) -> None:
    print(T)  # no diagnostic
    print(f"{T}")  # no diagnostic
    list[T]  # no diagnostic
    print(T == int)  # no diagnostic
    print(g[T]())  # no diagnostic
    Box(t)  # no diagnostic

uses[int](1)
uses[1](1)
uses[None](None)
uses[int | str](1)
uses[list[int]]([1])

def tests[T](x: object) -> bool:
    return x is T  # no diagnostic

assert tests[int](1)
assert not tests[None](1)
assert tests[int | str](1)
assert not tests[list[int]](1)
```

and it is assignable to a `TypeForm`

```by
from typing_extensions import TypeForm

def takes[U](form: TypeForm[U]) -> None: ...

def passes[T]() -> None:
    form: TypeForm[T] = T  # no diagnostic
    takes(T)  # no diagnostic

passes[int]()
```

## nothing on a type form is promised, `object`'s members included

a type argument can be a class, `None` or a union, and an attribute means something different on
each: `T.__repr__()` misses its instance when `T` is a class, and `None` has no `__module__`.
`getattr` reads the same attribute, so it fails the same way unless it is given a default

```by
def reads[T]() -> None:
    T.__repr__()  # error: [unresolved-attribute]
    T.__str__()  # error: [unresolved-attribute]
    T.__hash__()  # error: [unresolved-attribute]
    T.__eq__(1)  # error: [unresolved-attribute]
    T.__ne__(1)  # error: [unresolved-attribute]
    T.__sizeof__()  # error: [unresolved-attribute]
    T.__module__  # error: [unresolved-attribute]
    T.__dict__  # error: [unresolved-attribute]
    # error: [unresolved-attribute] "Object of type `TypeForm[T]` has no attribute `__name__`"
    getattr(T, "__name__")
    T.__doc__ = "x"  # error: [unresolved-attribute]
    del T.__doc__  # error: [unresolved-attribute]

def stored[T]() -> None:
    held = T
    held.__repr__()  # error: [unresolved-attribute]

def elements[*Ts]() -> None:
    for element in Ts:
        element.__repr__()  # error: [unresolved-attribute]

def optional[T](flag: bool) -> None:
    held = T if flag else None
    held.__repr__()  # error: [unresolved-attribute]
```

narrowing to a class gives the attributes of a class

```by
def narrowed[T]() -> object:
    if isinstance(T, type):
        T.__module__  # no diagnostic
        return getattr(T, "__name__")  # no diagnostic
    return None

assert narrowed[int]() == "int"
assert narrowed[None]() is None
```

what works on every value still does, because it asks the value's type rather than the value

```by
def everywhere[T]() -> None:
    print(repr(T))  # no diagnostic
    print(str(T))  # no diagnostic
    print(hash(T))  # no diagnostic
    type(T)  # no diagnostic
    print(hasattr(T, "__name__"))  # no diagnostic
    print(getattr(T, "__name__", "no name"))  # no diagnostic

everywhere[int]()
everywhere[None]()
everywhere[int | str]()
```

## a class base written as a type form is named as one

```by
def base[T]() -> None:
    # error: [invalid-base] "Invalid class base with type `TypeForm[T]`"
    class C(T): ...
```

## a guard only changes what the value read is

`isinstance(T, type)` is the route to `T()`, and a type expression inside it still means the type
parameter rather than the narrowed value

```by
def g[U]() -> object:
    return U

def guarded[T](x: object) -> object:
    if isinstance(T, type):
        print(x is T)  # no diagnostic
        print(x is list[T])  # no diagnostic
        print(g[T]())  # no diagnostic
        y: list[T] = []  # no diagnostic
        return T()
    print(x is T)  # no diagnostic
    print(g[T]())  # no diagnostic
    return None

assert guarded[int](1) == 0
assert guarded[None](1) is None
assert guarded[int | str](1) is None
```

## a single-member enum's member is a type argument

an enum with one member keeps its member as a type argument, as one with several does

```by
from enum import Enum

class Only(Enum):
    RED = 1

class Out[out T]: ...

def f[T]() -> object:
    return T

only: Only.RED = Only.RED  # no diagnostic
Out[Only.RED]()  # no diagnostic
print(f[Only.RED]())  # no diagnostic
```

## a type argument the test of a type parameter cannot ask about

`x is T` asks the runtime about whatever `T` holds, so a type argument with no runtime test is
refused where it is given, for the reason `x is UserId` is refused where that is written

```by
from typing import Annotated, Any, Callable, LiteralString, NewType, Self, TypedDict, TypeGuard, TypeVar

UserId = NewType("UserId", int)

class TD(TypedDict):
    a: int

LT = TypeVar("LT")
type Alias = UserId

def chk[T](x: object) -> bool:
    return x is T

# error: [erased-type-check] "`is UserId` cannot be checked at runtime: `UserId` has no runtime form"
chk[UserId](1)
chk[TD]({"a": 1})  # error: [erased-type-check]
chk[LiteralString]("a")  # error: [erased-type-check]
chk[TypeGuard[int]](1)  # error: [erased-type-check]
chk[UserId | None](1)  # error: [erased-type-check]
chk[Annotated[UserId, "x"]](1)  # error: [erased-type-check]
chk[Alias](1)  # error: [erased-type-check]
chk[Any](1)  # error: [erased-type-check]
chk[Callable[[int], str]](1)  # error: [erased-type-check]

def legacy(x: LT) -> LT:
    chk[LT](x)  # error: [erased-type-check]
    return x

class Own:
    def m(self) -> bool:
        return chk[Self](self)  # error: [erased-type-check]

# a specialization is refused where it is written, whatever it goes on to do
stored = chk[UserId]  # error: [erased-type-check]
```

## a protocol held by a type parameter

a protocol that isn't `@runtime_checkable` is tested member by member through a type parameter, as
it is in place, against the annotations of the value's class

```by
from typing import Protocol, runtime_checkable

class P(Protocol):
    a: int

class G[T](Protocol):
    a: T

@runtime_checkable
class R(Protocol):
    a: int

class Has:
    a: int = 1

class Other:
    a: str = "x"

def chk[T](x: object) -> bool:
    return x is T

def place(x: object) -> bool:
    return x is P

assert chk[P](Has())  # no diagnostic
assert not chk[P](Other())
assert not chk[P](1)
assert chk[G[int]](Has())  # no diagnostic
assert not chk[G[int]](Other())
assert chk[R](Has())  # no diagnostic
assert place(Has())
assert not place(Other())
```

## a `private` class of another module held by an alias

another module decides the name a `private` class of its own is emitted under, so nothing here can
say which specialization of it a test asks about. the test is refused in place and through a type
parameter

`dep.by`:

```by
private class Defaulted[T = int]: ...

private class Plain: ...

class Public[T = int]: ...

B = Defaulted | None
C = Plain | None
D = Public | None
E = Defaulted
```

`main.by`:

```by
import dep

def chk[T](x: object) -> bool:
    return x is T

def forwards[U](x: object) -> bool:
    return chk[U](x)

def shows[T]() -> object:
    return T

def tests[T](x: T, y: object) -> bool:
    return y is T

def hands_on[U](x: U) -> bool:
    return tests(x, 1)

def in_place(x: object) -> bool:
    # error: [erased-type-check] "`is dep.B` cannot be checked at runtime: `dep.B` has no runtime form"
    return x is dep.B

# error: [erased-type-check] "`is dep.B` cannot be checked at runtime: `dep.B` has no runtime form"
chk[dep.B](1)
chk[dep.C](1)  # error: [erased-type-check]
forwards[dep.B](1)  # error: [erased-type-check]
chk[dep.D](1)  # no diagnostic
shows[dep.B]()  # no diagnostic

# a name that evaluates to the class itself spells it, in place and through a type parameter
def spelled(x: object) -> bool:
    return x is dep.E  # no diagnostic

chk[dep.E](1)  # no diagnostic

def given(value: dep.B) -> None:
    tests(value, 1)  # error: [unspecialized-reified-generic]
    hands_on(value)  # error: [unspecialized-reified-generic]
```

## a class whose default asks for a subscript the runtime doesn't have

`memoryview` has a type parameter with a default, so even bare it asks for a specialization, and it
can't be subscripted before python 3.14. dropping the type arguments doesn't help, `isinstance` does

```toml
[environment]
python-version = "3.13"
```

```by
def chk[T](x: object) -> bool:
    return x is T

chk[memoryview](1)  # snapshot: erased-type-check

def in_place(x: object) -> bool:
    return x is memoryview  # snapshot: erased-type-check

def written(x: object) -> bool:
    return x is memoryview[bytes]  # snapshot: erased-type-check

def plain(x: object) -> bool:
    return isinstance(x, memoryview)  # no diagnostic
```

```snapshot
error[erased-type-check]: `is memoryview` cannot be checked at runtime: the target class cannot be subscripted
 --> src/mdtest_snippet.by:4:5
  |
4 | chk[memoryview](1)  # snapshot: erased-type-check
  |     ^^^^^^^^^^
info: `memoryview` has no `__class_getitem__` on this python version, and a type parameter with a default makes it ask for a specialization even written bare
info: test with `isinstance(value, memoryview)`, which asks only about the class
info: `chk` tests a value against its type parameter `T`


error[erased-type-check]: `is memoryview` cannot be checked at runtime: the target class cannot be subscripted
 --> src/mdtest_snippet.by:7:12
  |
7 |     return x is memoryview  # snapshot: erased-type-check
  |            ^^^^^^^^^^^^^^^
info: `memoryview` has no `__class_getitem__` on this python version, and a type parameter with a default makes it ask for a specialization even written bare
info: test with `isinstance(value, memoryview)`, which asks only about the class


error[erased-type-check]: `is memoryview[bytes]` cannot be checked at runtime: the target class cannot be subscripted
  --> src/mdtest_snippet.by:10:12
   |
10 |     return x is memoryview[bytes]  # snapshot: erased-type-check
   |            ^^^^^^^^^^^^^^^^^^^^^^
info: `memoryview` has no `__class_getitem__` on this python version, and a type parameter with a default makes it ask for a specialization even written bare
info: test with `isinstance(value, memoryview)`, which asks only about the class
```

## a type argument forwarded to a test

the refusal follows the type parameter to whatever tests it

```by
from typing import NewType

UserId = NewType("UserId", int)

def chk[T](x: object) -> bool:
    return x is T

def subscripts[U](x: object) -> bool:
    return chk[U](x)

def unions[U](x: object) -> bool:
    return chk[U | None](x)

def twice[U](x: object) -> bool:
    return subscripts[U](x)

def casts[T](x: object) -> T:
    return x cast! T

def try_casts[T](x: object) -> T | None:
    return x cast? T

subscripts[UserId](1)  # error: [erased-type-check]
unions[UserId](1)  # error: [erased-type-check]
twice[UserId](1)  # error: [erased-type-check]
casts[UserId](1)  # error: [erased-type-check]
try_casts[UserId](1)  # error: [erased-type-check]
```

## a type argument given by name, or past a variadic

```by
from typing import NewType

UserId = NewType("UserId", int)

def chk[T](x: object) -> bool:
    return x is T

def after[*Ts, T](x: object) -> bool:
    return x is T

chk[T=UserId](1)  # error: [erased-type-check]
after[int, str, UserId](1)  # error: [erased-type-check]
after[UserId, int](1)  # no diagnostic
```

## an override that tests what its base doesn't

a specialization through the base reaches the override, so the base's interface has to say so

```by
class Base:
    def shows[T](self, x: object) -> object:
        return T

    def tests[T](self, x: object) -> object:
        return x is T

class Sub(Base):
    # error: [invalid-method-override]
    def shows[T](self, x: object) -> object:
        return x is T

    def tests[T](self, x: object) -> object:  # no diagnostic
        return x is T
```

## a checked cast reads the type parameter

a checked cast tests its value against what the type parameter holds, so it reifies the parameter as
`x is T` does

```by
def casts[T](x: object) -> T:
    return x cast! T

def try_casts[T](x: object) -> T | None:
    return x cast? T

assert casts[int](1) == 1
assert try_casts[int](1) == 1
assert try_casts[str](1) is None
```

## a type argument forwarded to a test the checker solved

a parameter handed on through a call the checker solves is tested as much as one handed on in
writing. a type with no spelling has to be named by the call, and a type with a spelling that the
runtime can't test is refused at the call as it would be if the call named it

```by
from typing import NewType, Protocol

UserId = NewType("UserId", int)

class P(Protocol):
    a: int

def tests[T](x: T, y: object) -> bool:
    return y is T

def hands_on[U](x: U) -> bool:
    return tests(x, 1)

hands_on[UserId](UserId(1))  # error: [erased-type-check]
tests(UserId(1), 1)  # error: [erased-type-check]

def given(value: P) -> None:
    tests(value, 1)  # error: [unspecialized-reified-generic]

def defaults[T = UserId](y: object) -> bool:
    return y is T

defaults(1)  # error: [erased-type-check]
defaults[int](1)  # no diagnostic
```

## a solved type argument is tested as the type the call writes

a literal the runtime can't compare is written as its class, so a test of the type parameter tests
the class. the bare call means what the subscript it lowers to means

```by
from typing import LiteralString

def tests[T](v: T, x: object) -> bool:
    return x is T

tests(1.5, 1.5)  # no diagnostic
tests(1.5 * 2, 3.0)  # no diagnostic
tests(1j, 1j)  # no diagnostic
tests("a".upper(), "A")  # no diagnostic

def named(flag: bool, text: LiteralString):
    a = 1.5
    tests(a, 1.5)  # no diagnostic
    b: float = 1.5
    tests(b, 1.5)  # no diagnostic
    c = 1.5 if flag else "a"
    tests(c, "a")  # no diagnostic
    tests(text, "a")  # no diagnostic
    tests(f"a{text}", "a")  # no diagnostic

def forwards[V](v: V) -> bool:
    return tests(v, 1)

forwards(1.5)  # no diagnostic

class Method:
    def m[S](self, v: S, x: object) -> bool:
        return x is S

Method().m(1.5, 2.5)  # no diagnostic

class Field[T]:
    def __init__(self, v: T) -> None: ...

    def has(self, x: object) -> bool:
        return x is T

Field(1.5).has(1.5)  # no diagnostic
Field("a".upper()).has("A")  # no diagnostic
```

## a solved type argument the runtime can't test is refused

the same goes for one that is written as itself, on its own or as an arm

```by
from typing import NewType

UserId = NewType("UserId", int)

def tests[T](v: T, x: object) -> bool:
    return x is T

tests(UserId(1), 1)  # error: [erased-type-check]

def arm(flag: bool):
    c = UserId(1) if flag else 1.5
    tests(c, 1)  # error: [erased-type-check]

def forwards[V](v: V) -> bool:
    return tests(v, 1)

forwards(UserId(1))  # error: [erased-type-check]

class Field[T]:
    def __init__(self, v: T) -> None: ...

    def has(self, x: object) -> bool:
        return x is T

Field(UserId(1))  # error: [erased-type-check]
```

## a type argument that is only printed or handed on is not tested

```by
from typing import LiteralString, NewType, TypedDict

UserId = NewType("UserId", int)

class TD(TypedDict):
    a: int

def show[T]() -> object:
    return T

def hands_on[U]() -> object:
    return show[U]()

def nested[T](x: object) -> bool:
    return x is list[T]

show[UserId]()  # no diagnostic
show[TD]()  # no diagnostic
show[LiteralString]()  # no diagnostic
hands_on[UserId]()  # no diagnostic
nested[UserId]([])  # no diagnostic

def chk[T](x: object) -> bool:
    return x is T

assert chk[int](1)
assert not chk[None](1)
assert chk[int | None](None)
assert chk[object](1)
assert not chk[list[UserId]](1)  # no diagnostic
```

## subscripting a method

a method's type arguments are written as a function's are, whether its body reads them or not

```by
class K:
    def erases[T](self, x: T) -> T:
        return x

    def reads[T](self, x: T) -> T:
        print(T)
        return x

    class def erases_on_class[T](cls, x: T) -> T:
        return x

reveal_type(K().erases[int](1))  # revealed: int
reveal_type(K().reads[int](1))  # revealed: int
reveal_type(K.erases_on_class[int](1))  # revealed: int
K().erases[str](1)  # error: [invalid-argument-type]
```

the receiver gives `Self` its argument, including that of a method of a generic class, so a method
with no parameter of its own is not generic

```by
class G[U]:
    def erases[T](self, x: T) -> T:
        return x

    def reads[T](self, x: T) -> T:
        print(T)
        return x

    def plain(self, x: int) -> int:
        return x

reveal_type(G[int]().erases[str]("a"))  # revealed: str
reveal_type(G[int]().reads[str]("a"))  # revealed: str
G[int]().plain[str](1)  # error: [not-subscriptable]
```

## subscripting a function in a `.py` file

python functions aren't subscriptable and a `.py` file is run as written, so the subscript raises. a
`.by` function that takes type arguments is lowered to one that accepts them

```toml
[environment]
python-version = "3.13"
```

`lib.by`:

```by
def reads[T](x: object) -> object:
    return T

def erases[T](x: object) -> object:
    return x
```

`main.py`:

```py
from lib import reads, erases

def own[T](x: object) -> object:
    return x

class C:
    def m[T](self, x: object) -> object:
        return x

own[int](1)  # error: [not-subscriptable]
stored = own[int]  # error: [not-subscriptable]
C().m[int](1)  # error: [not-subscriptable]
reads[int](1)  # no diagnostic
erases[int](1)  # error: [not-subscriptable]
```

## the elements of a variadic and a pack are type forms

a `*Ts` holds the run of type arguments and a `**Kwargs` holds its fields, and each of those can be
any type expression too

```by
def run[*Ts]() -> None:
    reveal_type(Ts)  # revealed: (*: TypeForm[object])
    Ts[0]()  # error: [call-non-callable]

def fields[**Kwargs]() -> None:
    reveal_type(Kwargs)  # revealed: dict[str, TypeForm[object]]
    Kwargs["a"]()  # error: [call-non-callable]

def bounded[*Ts: int]() -> None:
    reveal_type(Ts)  # revealed: (*: TypeForm[object])
    Ts[0]()  # error: [call-non-callable]
```

## an element of a variadic or a pack narrows like any type form

```by
def first[*Ts]() -> object:
    head = Ts[0]
    if isinstance(head, type):
        reveal_type(head)  # revealed: type
        return head()
    return None

assert first[int]() == 0
assert first[None]() is None
assert first[int | str]() is None

def field[**Kwargs]() -> object:
    value = Kwargs["a"]
    if isinstance(value, type):
        reveal_type(value)  # revealed: type
        return value()
    return None

assert field[a=int]() == 0
assert field[a=None]() is None
```

## a value of `type[T]` is a class

a parameter annotated `type[T]` is given a class by whoever calls it, and only a read of the type
parameter is a type form

```by
def make[T](t: type[T]) -> T:
    reveal_type(t)  # revealed: type[T]
    return t()

assert make(int) == 0
```

## a legacy `TypeVar` is the `TypeVar` object

outside a `.by` file a read of a type parameter isn't reified, and a legacy `TypeVar` never is

```toml
[environment]
python-version = "3.12"
```

```py
from typing import TypeVar

T = TypeVar("T")

reveal_type(T)  # revealed: TypeVar

def make(t: type[T]) -> T:
    reveal_type(T)  # revealed: TypeVar
    return t()

def f[U]() -> None:
    reveal_type(U)  # revealed: TypeVar
```

## `isinstance` against a reified parameter

`isinstance`'s second argument is a value position, so `T` reifies. `isinstance` takes a class,
which is what narrowing makes it:

```by
def f[T](t: object) -> bool:
    return isinstance(T, type) and isinstance(t, T)

reveal_type(f[int](1))  # revealed: bool
```

## annotation-only use is not reified

A type parameter used only in annotations stays erased and remains a plain generic — the reference
in type position is the `TypeVar`, not a value:

```by
def f[T](x: T) -> T:
    return x

reveal_type(f[int](1))  # revealed: int
```

## a parameter of the same name is not a read of the type parameter

A parameter is bound in the body's scope and the type parameter in the one around it, so the body
reads the parameter under that name. `n` here is used only in annotations, stays erased, and the
call needs no specialization even though nothing solves `n`:

```by
def f[n](n: n | None = None) -> n | None:
    reveal_type(n)  # revealed: n | None
    return n

f()
```

## `reified` declares reification outright

The `reified` modifier reifies a type parameter whether or not the body ever reads it as a value.
That makes the runtime value part of the declaration rather than a consequence of how the body
happens to be written:

```by
def f[reified T]():
    pass

f[int]()
```

## a declared parameter still requires its specialization

Reification is what makes the `[...]` step mandatory, so a declared parameter that no argument
solves has to be written out:

```by
def f[reified T]():
    pass

# error: [unspecialized-reified-generic] "Cannot call reified generic function `f` without explicit specialization"
f()
```

## a declared parameter is solved from the arguments like any other

```by
def f[reified T](t: T) -> T:
    return t

f(1)  # runs as f[int](1)
reveal_type(f(""))  # revealed: ""
```

## packs take the modifier too

```by
def f[reified *Ts, reified **Kwargs]():
    pass

f[int, str]()
f[foo=int]()
f()
```

## `reified` beside plain parameters

Only the declared parameter reifies; the rest stay erased, so nothing about them changes:

```by
def f[reified T, U](u: U) -> U:
    return u

reveal_type(f[int, str](""))  # revealed: str
```

## a declared reified classmethod is an error

The modifier reaches the same rule a value-position use does — the classmethod binding has no
closure to rebuild:

```by
class C:
    @classmethod
    # error: [reified-classmethod]
    def f[reified T](cls) -> None:
        pass
```

## a type alias type parameter cannot be reified

```by
# error: [invalid-reified-type-param]
type Alias[reified T] = list[T]
```

## a stub declares the reified interface

A stub says what a runtime function's interface is, and reification is part of it — so the modifier
is honoured there and callers still have to specialize:

`m.byi`:

```byi
def f[reified T](t: T) -> T: ...
```

```by
from m import f

reveal_type(f[int](1))  # revealed: int
```

## a `ParamSpec` has nothing to reify

`**P` is a keyword-variadic pack only in a `.by` source file. Everywhere else it declares a PEP 612
`ParamSpec` — a parameter list, which has no runtime object to bind:

`m.byi`:

```byi
def f[reified **P]() -> None: ...  # error: [invalid-reified-type-param] "Type parameter `P` cannot be reified"
```

```by
from m import f

f()
```

## a `type def` type parameter cannot be reified

A `type def` is erased by the transpiler — its type parameters are the type arguments of an
application, not runtime values:

```by
# error: [invalid-reified-type-param]
type def Pair[reified T]:
    return tuple[T, T]
```

## specialization is inferred from arguments

When a reified type parameter appears in the signature, a bare call reifies it through inference:
the transpiler injects the statically inferred specialization at the call site (`f(1)` →
`f[int](1)`), promoting literal solutions to their runtime class first:

```by
def f[T](t: T) -> T:
    print(1 is T)
    return t

f(1)   # runs as f[int](1)
f("")  # runs as f[str]("")
reveal_type(f(True))  # revealed: True
```

Structured annotations solve through their type arguments; unions and tuples spell as runtime
expressions; keyword arguments participate like positional ones:

```by
def elem[T](ts: list[T]) -> None:
    print(T)

elem([1])       # runs as elem[int]([1])
elem(ts=["a"])  # runs as elem[str](ts=["a"])

def pick[T](t: T) -> None:
    print(T)

def choose(flag: bool) -> None:
    pick(1 if flag else "")  # runs as pick[int | str](…)

choose(True)
pick((1, "a"))  # runs as pick[tuple[int, str]]((1, "a"))
```

An inferred solution beats a PEP 696 default, and an erased type parameter may stay unsolved when it
needs no runtime value:

```by
def d[T = int](t: T) -> None:
    print(T)

d("")  # runs as d[str]("") — the argument wins over the default
d(0)   # runs as d[int](0)

def partial[T, U](t: T) -> None:
    print(T)

partial(1)  # runs as partial[int](1) — erased `U` needs no value
```

## when inference cannot reify

The injected specialization must be a *runtime expression* that evaluates to the intended type at
the call site. A type parameter that never appears in the signature, a solution without a runtime
spelling (a scope-local class), or arguments hidden behind unpacking keep the bare call an error:

```by
def f[T](t: object) -> bool:
    return t is T

f[int](1)  # ok
# error: [unspecialized-reified-generic] "Cannot call reified generic function `f` without explicit specialization"
f(1)

def g[T](t: T) -> None:
    print(T)

def local() -> None:
    class Hidden: ...
    # error: [unspecialized-reified-generic]
    g(Hidden())

args = (1,)
# error: [unspecialized-reified-generic]
g(*args)
```

## a PEP 696 default fills the reified slot

A type parameter with a default supplies the reified value when the specialization is omitted, so
the bare call is allowed:

```by
def f[T = int]():
    reveal_type(T)  # revealed: TypeForm[T]

f()       # ok — default supplies the reified value
f[str]()  # ok
```

## a `NewType` fills the reified slot

a `NewType` has a spelling, the name that holds it, so it fills the slot as a class does, alone or
inside a larger type, whether a default or the arguments solve it

```by
from typing import NewType

UserId = NewType("UserId", int)

def alone[T = UserId]():
    return T

def nested[T = list[UserId]]():
    return T

def optional[T = UserId | None]():
    return T

def solved[T](x: T):
    return T

alone()  # no diagnostic
alone[UserId]()  # no diagnostic
nested()  # no diagnostic
optional()  # no diagnostic
solved(UserId(1))  # no diagnostic
solved([UserId(1)])  # no diagnostic

Derived = NewType("Derived", UserId)

def derived[T = Derived]():
    return T

derived()  # no diagnostic
```

## a nearer binding hides the name a bare call would write

a bare call is written with the type arguments the checker solved, each read by a name where the
call runs. a nearer binding of that name would hand the callee something else, so the call has to
name them itself, whatever the kind of the type and of the binding

```by
from enum import Enum
from typing import Literal, NewType

U = NewType("U", int)

class K: ...

class Color(Enum):
    RED = 1

def of_class[T = K]():
    return T

def of_new_type[T = U]():
    return T

def of_nested[T = list[K]]():
    return T

def solved[T](x: T):
    return T

class Default[T = K]:
    def read(self) -> object:
        return T

class Held[T]:
    def __init__(self, x: T): ...

    def read(self) -> object:
        return T

u = U(1)
k = K()

def local_assignment():
    K = 5
    U = 5
    of_class()  # error: [unspecialized-reified-generic]
    of_new_type()  # error: [unspecialized-reified-generic]
    of_nested()  # error: [unspecialized-reified-generic]
    Default()  # error: [unspecialized-reified-generic]
    solved(k)  # error: [unspecialized-reified-generic]
    solved(u)  # error: [unspecialized-reified-generic]
    Held(k)  # error: [unspecialized-reified-generic]
    Held(u)  # error: [unspecialized-reified-generic]

def local_of_another_kind():
    K = str
    U = NewType("U", str)
    of_class()  # error: [unspecialized-reified-generic]
    of_new_type()  # error: [unspecialized-reified-generic]

def parameter(K: int, U: int):
    of_class()  # error: [unspecialized-reified-generic]
    of_new_type()  # error: [unspecialized-reified-generic]

def enclosing():
    K = 5

    def inner():
        return of_class()  # error: [unspecialized-reified-generic]

    return inner()

def enum_member(member: Literal[Color.RED]):
    Color = 5
    solved(member)  # error: [unspecialized-reified-generic]

class ClassBody:
    K = 5
    default = of_class()  # error: [unspecialized-reified-generic]

[of_class() for K in [1]]  # error: [unspecialized-reified-generic]

(lambda K=1: of_class())()  # error: [unspecialized-reified-generic]

def type_parameter[K](x: K):
    of_class()  # error: [unspecialized-reified-generic]
    solved(k)  # error: [unspecialized-reified-generic]

class TypeParameter[U]:
    def m(self):
        return solved(u)  # error: [unspecialized-reified-generic]

def builtin(v: int, w: list[int]):
    int = 5
    solved(v)  # error: [unspecialized-reified-generic]

def builtin_outer(v: int):
    list = 5
    solved([v])  # error: [unspecialized-reified-generic]
```

## a binding a bare call can't see hides nothing

```by
from typing import NewType

U = NewType("U", int)

class K: ...

def of_class[T = K]():
    return T

def of_new_type[T = U]():
    return T

def solved[T](x: T):
    return T

class Attribute:
    K = 5
    U = 5

    def m(self):
        of_new_type()  # no diagnostic
        return of_class()  # no diagnostic

def another_function():
    K = 5
    U = 5

def uses_globals(v: U):
    global K
    of_class()  # no diagnostic
    of_new_type()  # no diagnostic
    solved(v)  # no diagnostic

def comprehension_elsewhere():
    x = [K for K in [1]]
    of_class()  # no diagnostic

def builtin(v: int):
    return solved(v)  # no diagnostic
```

## a `NewType` has to be held by a name where the call runs

the name has to hold the `NewType` where the call runs, and it has to be a name of the module that
makes the call

```by
from typing import NewType

def too_early[T = Later]():
    return T

too_early()  # error: [unspecialized-reified-generic]

Later = NewType("Later", int)

def local():
    Local = NewType("Local", int)

    def inner[T = Local]():
        return T

    inner()  # error: [unspecialized-reified-generic]
    inner[Local]()  # no diagnostic
```

## a `NewType` of another module has to be imported

the module that makes the call has to have a name for it, as it has to for a class

`defines.by`:

```by
from typing import NewType

Elsewhere = NewType("Elsewhere", int)

def default_elsewhere[T = Elsewhere]():
    return T
```

`imports_it.by`:

```by
from defines import Elsewhere, default_elsewhere

default_elsewhere()  # no diagnostic
```

`imports_the_function_only.by`:

```by
from defines import default_elsewhere

default_elsewhere()  # error: [unspecialized-reified-generic]
```

## a missing default is still required when another defaults

Only a trailing run of defaulted parameters may be omitted; a non-defaulted reified parameter still
demands specialization:

```by
def f[T, U = str]():
    reveal_type(T)  # revealed: TypeForm[T]
    reveal_type(U)  # revealed: TypeForm[U]

f[int]()  # ok
# error: [unspecialized-reified-generic] "Cannot call reified generic function `f` without explicit specialization"
f()
```

## a reified method

A method whose type parameter is used in a value position reifies the same way. The receiver binds
through the wrapper's descriptor at runtime; the type checker still requires the `[...]`
specialization:

```by
class Box:
    def kind[T](self) -> object:
        print(T)
        return T

reveal_type(Box().kind[int]())  # revealed: object
# error: [unspecialized-reified-generic] "Cannot call reified generic function `kind` without explicit specialization"
Box().kind()
```

## reified and erased are distinct

A reified generic is structurally a two-step callable (`f[...]` then `(...)`). A plain callable has
no slot for the specialization step, so a reified generic is not assignable to one:

```by
def f[T]():
    print(T)

# error: [invalid-assignment]
c: () -> None = f
```

An erased generic (type parameter used only in annotations) keeps its PEP 695 assignability and
remains usable wherever a plain callable is expected:

```by
def g[T](x: T) -> T:
    return x

c: (int) -> int = g  # ok — erased generic specializes to a plain callable
```

## a reified generic keeps its type when stored

a type inferred from a stored value is widened, but a `.by` module keeps a function's own type
rather than widening it to a callable type, which has no slot for the specialization step either. so
a reified generic can still be specialized once it is read back out of a list, a dict, a tuple or an
attribute

```by
def name[reified T]() -> str:
    return str(T)

class Holder:
    def __init__(self):
        self.f = name

listed = [name]
keyed = {"a": name}
paired = (name, 1)
held = Holder()
appended = []
appended.append(name)

reveal_type(listed)  # revealed: list[def name[T]() -> str]
reveal_type(held.f)  # revealed: def name[T]() -> str
reveal_type(listed[0][int]())  # revealed: str
reveal_type(keyed["a"][str]())  # revealed: str
reveal_type(paired[0][bytes]())  # revealed: str
reveal_type(held.f[float]())  # revealed: str
reveal_type(appended[0][int]())  # revealed: str
```

## overrides must keep the reified interface

Specializations flow through the base type — `a.f[int]()` on `a: A` dispatches to the override at
runtime — so an override must accept every specialization the base permits. Arity counts like value
parameters (PEP 696 defaults make a parameter optional), and names are positional:

```by
class A:
    def f[T](self) -> None:
        print(T)

class B(A):
    # error: [invalid-method-override]
    def f[A2, B2](self) -> None:
        print(A2, B2)

class C(A):
    def f[X](self) -> None:  # ok — names are positional
        print(X)

class D(A):
    def f[X, Y = str](self) -> None:  # ok — the extra parameter defaults
        print(X, Y)

class E(A):
    def f[X = bytes](self) -> None:  # ok — a default only widens what callers may omit
        print(X)
```

Bounds are contravariant — the override may widen what the base admits, never narrow it:

```by
class Narrow:
    def f[T: int](self) -> None:
        print(T)

class Wider(Narrow):
    def f[X](self) -> None:  # ok — accepts everything the base does
        print(X)

class Base:
    def g[T](self) -> None:
        print(T)

class Narrower(Base):
    # error: [invalid-method-override]
    def g[X: int](self) -> None:
        print(X)
```

Reified and erased stay distinct across an override. Erasing a reified method would make `f[...]`
through the base subscript a plain function; reifying an erased one demands values a bare call
through the base cannot supply — unless every reified parameter defaults:

```by
class R:
    def f[T](self) -> None:
        print(T)

class Erases(R):
    # error: [invalid-method-override]
    def f(self) -> None: ...

class Plain:
    def g[T](self, t: T) -> None: ...

class Reifies(Plain):
    # error: [invalid-method-override]
    def g[T](self, t: T) -> None:
        print(T)

class ReifiesWithDefault(Plain):
    def g[T = int](self, t: T) -> None:  # ok — a bare call falls back to the default
        print(T)
```

## `is` composes with reification

basedpython `is` means `isinstance`, and a reified `T` is a runtime class, so the two compose:

```by
def f[T]() -> None:
    print(1 is T)

f[int]()  # prints True
f[str]()  # prints False
```

## reification preserves the rest of the closure

Only the cells named after type parameters are rebuilt; a captured outer local and the implicit
`__class__` cell behind zero-arg `super()` carry over untouched:

```by
def outer() -> object:
    x = 5
    def f[T]() -> object:
        print(T)
        return x
    return f[int]()

outer()

class Base:
    def m(self) -> str:
        return "base"

class Sub(Base):
    def probe[T](self) -> str:
        print(T)
        return super().m()

Sub().probe[int]()
```

## partially-used type parameters

Only the type parameters referenced in a value position reify; the others stay erased but still
occupy their specialization slot:

```by
def f[T, U](x: U) -> U:
    print(T)
    return x

reveal_type(f[int, str]("a"))  # revealed: str
```

## parameter defaults are preserved

Specialization rebuilds the function; its parameter defaults (positional and keyword-only) still
apply:

```by
def f[T](x: int = 5, *, key: str = "k") -> int:
    print(T, key)
    return x

reveal_type(f[int]())  # revealed: int
```

## specialization arity is checked

```by
def f[T]() -> None:
    print(T)

f[int, str]()  # error: [invalid-type-arguments]
```

## a variadic type parameter reifies to the tuple of its arguments

A `*Ts` parameter takes a whole *run* of type arguments rather than one, so its runtime value is the
tuple of them — and the arity of a parameter list containing one is unbounded:

```by
def f[T, *Args]() -> None:
    reveal_type(T)  # revealed: TypeForm[T]
    reveal_type(Args)  # revealed: (*: TypeForm[object])
    assert T === int
    assert Args == (str, bool)

f[int, str, bool]()
```

## a variadic absorbs only what the fixed parameters leave

The parameters before and after the variadic claim their arguments from the front and the back; the
variadic takes the rest, down to nothing:

```by
def f[A, *Rest, Z]() -> None:
    assert (A, Rest, Z) == (int, (str, bytes), bool) or (A, Rest, Z) == (int, (), bool)

f[int, str, bytes, bool]()
f[int, bool]()
```

## a variadic on its own reifies

A variadic is a reifying reference like any other type parameter — it does not need a plain
parameter alongside it:

```by
def f[*Ts]() -> None:
    assert Ts == (int, str) or Ts == ()

f[int, str]()
f()
```

## a variadic PEP 696 default fills the slot

The default is a run of arguments too, so it spells as that many arguments at the injected call
site:

```by
def f[*Ts = *tuple[int, str]]() -> None:
    assert Ts == (int, str) or Ts == (bool,)

f()
f[bool]()
```

## a variadic is solved from the arguments

A variadic never forces the specialization step the way a plain reified parameter does: supplying it
nothing is a complete answer, not a missing one, so a bare call stays legal. The run it binds is
solved from the arguments, the same way a lone type parameter and a keyword pack are, so writing the
step out and leaving it off reach the same answer:

```by
def f[*Ts](*args: *Ts) -> None:
    assert Ts == (int, str)

f(int(1), str("a"))
f[int, str](1, "a")
```

Each element of the run is what the checker solved for that argument, so a literal stays one. Equal
floats can be different values (`0.0 == -0.0`), so the runtime can't tell one float literal from
another, and `2.0` binds `float` — not the `int | float` that a float argument is merely *accepted*
as:

```by
from typing import Literal

def g[*Ts](*args: *Ts) -> None:
    assert Ts == (Literal[1], float)

g(1, 2.0)
```

Inference can only fill the step with types that have a runtime spelling at the call site. A class
local to a function does not, so rather than bind a run naming something the call cannot see, the
call is rejected and the step has to be written out:

```by
def h[*Ts](*args: *Ts) -> None:
    print(Ts)

def make() -> None:
    class Local: ...
    # error: [unspecialized-reified-generic]
    h(Local())
```

## a keyword-variadic pack reifies to its fields

A `**Kwargs` pack is an ordered mapping of field name to type, so its runtime value is that mapping.
Python's subscript grammar takes no keywords, so a keyword specialization lowers to the wrapper's
`__getitem__` call:

```by
def f[**Kwargs]() -> None:
    reveal_type(Kwargs)  # revealed: dict[str, TypeForm[object]]
    assert Kwargs == {"foo": int, "bar": str}

f[foo=int, bar=str]()
```

## a pack sits outside the positional slots

The other type parameters are given positionally and the pack takes the keyword fields, whatever
order they are written in:

```by
def f[T, **Kwargs]() -> None:
    assert T === int
    assert Kwargs == {"foo": str}

f[int, foo=str]()
```

## a variadic and a pack in one list

The pack occupies no positional slot, so a variadic beside one still absorbs every positional type
argument the fixed parameters don't claim:

```by
def f[*Args, **Kwargs]() -> None:
    assert Args == (int, str)
    assert Kwargs == {"a": int, "b": str}

f[int, str, a=int, b=str]()
```

## an unfilled pack is empty

Like a variadic, a pack never forces the specialization step — supplying it no fields is a complete
answer:

```by
def f[**Kwargs]() -> None:
    assert Kwargs == {}

f()
```

## a pack is inferred from keyword arguments

`**kwargs: **Kwargs` unpacks the pack into the parameter list, so a bare call's keyword arguments
solve it and the transpiler injects the solved fields:

```by
def f[**Kwargs](**kwargs: **Kwargs) -> None:
    assert Kwargs == {"a": int, "b": str}

f(a=1, b="x")  # runs as f[a=int, b=str](a=1, b="x")
```

## a reified cell forwards as a type argument

A caller that reifies `T` holds it in a runtime cell, so a bare call that solves the callee's
parameter to that same `T` is answerable: the cell forwards as the type argument. Without this a
specialization dies at the first hop that only passes a value along.

```by
def inner[reified T](data: list[T]) -> None:
    assert T == int

def outer[reified T](data: list[T]) -> None:
    inner(data)  # runs as inner[T](data)

outer(list[int]())
```

## a type parameter only forwarded is inferred to be reified

A parameter nothing else reifies gets a cell when the body hands it to something that needs one, so
a specialization survives every hop that only passes a value along. No keyword is needed:

```by
def inner[reified T](data: list[T]) -> None:
    assert T == int

def middle[T](data: list[T]) -> None:
    inner(data)  # runs as inner[T](data)

def outer[T](data: list[T]) -> None:
    middle(data)  # runs as middle[T](data)

outer(list[int]())
```

## a recursive call hands on its own type parameter

```by
def count[reified T](t: T, n: int) -> int:
    assert T == int
    return n if n == 0 else count(t, n - 1)  # no diagnostic

assert count(int(1), 2) == 0
```

## a type parameter holds the literal it was solved to

`f(1)` solves `T` to `1`, and that is what the cell holds. A test against the cell asks whether the
value is of that type, whatever kind of type it is:

```by
from typing import Literal

def holds[T](t: T) -> object:
    return T

assert holds(1) == Literal[1]
assert holds(int(1)) == int

def test[T](t: T, value: object) -> bool:
    return value is T

assert test(1, 1)
assert not test(1, 2)
assert not test(1, True)
assert test("a", "a")
assert not test("a", "b")
assert test(int(1), 2)
assert not test(int(1), "b")
assert not test(list[int](), 1)
assert test(None, None)
assert not test(None, 1)
```

A literal cell holds the `typing.Literal` the type argument is, so a body that narrows `T` to a
class never reads one as a class:

```by
from typing import Literal

def is_class[T](t: T) -> bool:
    return isinstance(T, type)

assert not is_class(1)
assert is_class(int(1))

def holds[T](t: T) -> object:
    return T

assert holds(1) == Literal[1]
assert holds(True) != Literal[1]
```

## a type parameter holds the class of a literal the runtime can't record

The runtime can't tell one float literal from an equal one, and `LiteralString` or a template
literal type isn't one value at all, so the cell holds the class instead:

```by
from typing import LiteralString

def holds[T](t: T) -> object:
    return T

def text(s: LiteralString) -> object:
    return holds(s)

def number(n: 1.5) -> object:
    return holds(n)

def pattern(s: f"a{int}b") -> object:
    return holds(s)

assert text("a") == str
assert number(1.5) == float
assert pattern("a1b") == str
```

## a name that hides a type parameter

A forwarded type argument is read by the type parameter's name, so a nearer binding of that name
hides it. The value then records nothing rather than recording what the name holds:

```by
class Box[T]:
    init(var t: T)

def hidden_by_a_parameter[T](t: T) -> object:
    def inner(T: int) -> object:
        return Box(t)  # no diagnostic
    return inner(1)

def hidden_by_a_class[T](t: T) -> object:
    class Holder:
        T = 1
        value = Box(t)
    return Holder.value

def hidden_by_a_lambda[T](t: T) -> object:
    return (lambda T: Box(t))(1)

for value in hidden_by_a_parameter("x"), hidden_by_a_class("x"), hidden_by_a_lambda("x"):
    assert not value is Box[1]
    assert not value is Box["x"]
```

A class's binding is only seen by the code written directly in it:

```by
class Box2[T]:
    init(var t: T)

def method_in_class[T](t: T) -> object:
    class Holder:
        T = 1
        def build(self) -> object:
            return Box2(t)
    return Holder().build()

assert method_in_class("x") is Box2["x"]
```

A callee that has to be given the type argument can't be, so that call is rejected:

```by
def read[reified T](t: T) -> None: ...

def hidden[T](t: T) -> None:
    def inner(T: int) -> None:
        # error: [unspecialized-reified-generic]
        read(t)
```

## an override of a method that forwards

A call through the base reaches the override. An override whose body needs its type argument can't
be reached by a call through a base that doesn't take one:

```by
from typing import override

def read[reified T](t: T) -> None: ...

class Base:
    def method[T](self, t: T) -> object:
        return t

class Requires(Base):
    @override
    def method[T](self, t: T) -> object:  # error: [invalid-method-override]
        read(t)
        return t
```

A specialization through the base fills the override's type parameters by position, so they have to
be in the same order. What they are called doesn't matter:

```by
from typing import override

class Box3[T]:
    init(var t: T)

class Pair:
    def method[A, B](self, a: A, b: B) -> object:
        return Box3(a)

class Swapped(Pair):
    @override
    def method[B, A](self, a: A, b: B) -> object:  # error: [invalid-method-override]
        return Box3(a)

class Renamed(Pair):
    @override
    def method[X, Y](self, a: X, b: Y) -> object:  # no diagnostic
        return Box3(a)
```

## a forwarded type parameter a callee reads has to be supplied

A hop that reads the value through its callee has to be given it, as a declared `reified` would:

```by
def inner[reified T](data: T) -> None: ...

def outer[T](data: T) -> None:
    inner(data)  # no diagnostic

def dynamic(x):
    # error: [unspecialized-reified-generic]
    outer(x)
```

## a classmethod has no cell to forward

The binding hides the function a type argument would be rebuilt into:

```by
def inner[reified T](data: T) -> None: ...

class C:
    @classmethod
    def m[T](cls, data: T) -> None:
        # error: [unspecialized-reified-generic]
        inner(data)
```

## an erased class type parameter has no cell to forward

Forwarding reads the parameter as a value, and a class parameter no method reads is erased — there
is nothing on the instance to forward. (A class that does reify one is a different matter; see
`basedpython_reified_class_generics.md`.)

```by
def inner[reified T](data: list[T]) -> None: ...

class C[T]:
    xs: list[T]

    def m(self) -> None:
        # error: [unspecialized-reified-generic]
        inner(self.xs)
```

## a variadic used only in annotations is not reified

```by
def f[*Ts](*args: *Ts) -> tuple[*Ts]:
    return args

reveal_type(f[int, str](1, "a"))  # revealed: (int, str)
```

## async and generator functions reify

The closure rebuild preserves the code object, so coroutine and generator functions specialize like
plain ones:

```by
import asyncio

async def af[T]() -> object:
    return T

def gen[T]():
    yield T

async def main() -> None:
    print(await af[int]())

asyncio.run(main())
print(list(gen[str]()))
```

## a reified staticmethod

The `generic` wrapper sits innermost — directly on the raw function — so `@staticmethod` composes
through its descriptor:

```by
class C:
    @staticmethod
    def f[T]() -> object:
        return T

reveal_type(C.f[int]())  # revealed: object
C().f[str]()
```

## a reified classmethod is an error

The classmethod binding hides the function whose closure holds the reified cells, so a classmethod
cannot reify — neither the specialization nor the bare call could work at runtime. The error is
reported at the definition:

```by
class C:
    @classmethod
    # error: [reified-classmethod]
    def f[T](cls) -> None:
        print(T)

# the impossible specialization falls through to the ordinary subscript error
C.f[int]()  # error: [not-subscriptable]
```

`__init_subclass__` and `__class_getitem__` are implicitly classmethods and get the same treatment:

```by
class D:
    # error: [reified-classmethod]
    def __init_subclass__[T](cls) -> None:
        print(T)
```
