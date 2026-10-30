# basedpython: an override's parameters take the overridden method's types

a method that overrides another accepts every call the overridden method accepts, so a parameter it
leaves unannotated has the type of the parameter whose arguments it receives. the body sees that
type, and so does every call.

```toml
[environment]
python-version = "3.12"
```

## a parameter takes the overridden method's type

```by
class A:
    def f(self, a: int): ...

class B(A):
    override def f(self, a):
        reveal_type(a)  # revealed: int

reveal_type(B().f)  # revealed: bound method B.f(a: int)

B().f(1)
B().f("s")  # error: [invalid-argument-type]
```

`A.f` writes no return type, so its receiver is a `Self` bound to `A.f` itself. that is no reason to
inherit nothing: the receiver is not one of the arguments `B.f` takes over, and `B.f` has a receiver
of its own.

## the method a class overrides, not the keyword

what a method overrides is a fact about the class it is defined in, so the parameter types reach it
whether or not it says `override`. leaving the keyword out is a separate matter, for
`missing-override-decorator`.

```by
class A:
    def f(self, a: int): ...

class B(A):
    def f(self, a):
        reveal_type(a)  # revealed: int
```

## an explicit annotation wins

```by
class A:
    def f(self, a: int, b: str): ...

class B(A):
    override def f(self, a: object, b):
        reveal_type(a)  # revealed: object
        reveal_type(b)  # revealed: str
```

## parameters correspond the way arguments bind to them

a parameter receives the arguments a call passes to its counterpart in the overridden method: a
positional-only parameter's by position, a keyword-only parameter's by name, and a
positional-or-keyword parameter's by both. so an override may rename a positional-only parameter, or
take a keyword-only one positionally too, and keep its type:

```by
class A:
    def f(self, a: int, /, *, k: str): ...

class B(A):
    override def f(self, x, /, k):
        reveal_type(x)  # revealed: int
        reveal_type(k)  # revealed: str
```

a positional-or-keyword parameter the override renames receives the base parameter's positional
arguments but not its keyword ones, so it takes nothing — and is an invalid override anyway:

```by
class A:
    def f(self, a: int): ...

class B(A):
    override def f(self, b):  # error: [invalid-method-override]
        reveal_type(b)  # revealed: b
```

## `*args` and `**kwargs`

each stands for a run of arguments rather than one, so each takes the type of the base's own `*args`
or `**kwargs`:

```by
class A:
    def f(self, a: int, *args: str, **kwargs: bytes): ...

class B(A):
    override def f(self, a, *args, **kwargs):
        reveal_type(args)  # revealed: (*: str)
        reveal_type(kwargs)  # revealed: dict[str, bytes]
```

## `*args` and `**kwargs` that also receive the parameters around them

an override that gathers what the base spells out receives those arguments in its run as well, so no
one parameter of the base describes it. this is how an override forwards everything to `super()`,
and it takes nothing, which leaves the forwarding valid:

```by
class A:
    def f(self, a: int, *args: str, **kwargs: bytes): ...
    def g(self, *, k: int, **kwargs: str): ...

class B(A):
    override def f(self, *args, **kwargs):
        reveal_type(args)  # revealed: (*: Unknown)
        reveal_type(kwargs)  # revealed: dict[str, Unknown]
        return super().f(*args, **kwargs)  # no diagnostic

    override def g(self, **kwargs):
        reveal_type(kwargs)  # revealed: dict[str, Unknown]

B().f(1, "s")  # no diagnostic
B().g(k=1)  # no diagnostic
```

the same goes for a parameter of the override that the base's run feeds:

```by
class C:
    def f(self, *args: int): ...

class D(C):
    override def f(self, a, *args):  # error: [invalid-method-override]
        reveal_type(a)  # revealed: a
        reveal_type(args)  # revealed: (*: int)
```

## an annotation that describes the whole run

`*args: *Ts`, `*args: *tuple[int, str]`, `*args: P.args`, `**kwargs: P.kwargs` and
`**kwargs: Unpack[TD]` say something about all of the arguments at once rather than about each, so
there is no type of each argument to take:

```by
class A[*Ts]:
    def f(self, *args: *Ts): ...

class B[*Us](A[*Us]):
    override def f(self, *args):
        reveal_type(args)  # revealed: (*: Unknown)

class C:
    def f(self, *args: *tuple[int, str]): ...

class D(C):
    override def f(self, *args):
        reveal_type(args)  # revealed: (*: Unknown)
```

```py
from typing import Generic, ParamSpec, TypedDict, Unpack

P = ParamSpec("P")

class A(Generic[P]):
    def f(self, *args: P.args, **kwargs: P.kwargs): ...

class B(A[P]):
    def f(self, *args, **kwargs):
        reveal_type(args)  # revealed: tuple[Unknown, ...]
        reveal_type(kwargs)  # revealed: dict[str, Unknown]

class TD(TypedDict):
    x: int

class K:
    def f(self, **kwargs: Unpack[TD]): ...

class L(K):
    def f(self, **kwargs):
        reveal_type(kwargs)  # revealed: dict[str, Unknown]
```

## `staticmethod` and `classmethod`

a receiver the call binds is not an argument, so it corresponds to nothing, and the parameters after
it line up whatever kind of method each side is.

```by
class A:
    @staticmethod
    def s(a: int): ...
    @classmethod
    def c(cls, a: int): ...
    def m(self, a: int): ...

class B(A):
    @staticmethod
    override def s(a):
        reveal_type(a)  # revealed: int
    @classmethod
    override def c(cls, a):
        reveal_type(a)  # revealed: int
    @staticmethod
    override def m(a):
        reveal_type(a)  # revealed: int
```

## every base that defines the method

a method overrides the method of that name in every class of its MRO that defines one, and the
override checks hold it to each of them. so its parameter accepts the arguments of each, and takes
the union of their types:

```by
class A:
    def f(self, a: str): ...

class B:
    def f(self, a: bytes): ...

class C(A, B):
    override def f(self, a):
        reveal_type(a)  # revealed: str | bytes
```

where one base already overrides the other, the union is the overriding one's own type, which
accepts everything the other's does:

```by
class A:
    def f(self, a: int): ...

class B(A):
    override def f(self, a: object): ...

class C(B):
    override def f(self, a):
        reveal_type(a)  # revealed: object
```

a base that hands a parameter arguments of several of its own parameters describes it with none of
them, so the parameter takes nothing rather than the type of the other base, which would reject the
arguments of that one:

```by
class A:
    def f(self, a: int, /, *, x: str): ...

class B:
    def f(self, x: bytes): ...

class C(A, B):
    def f(self, x):  # error: [invalid-method-override]
        reveal_type(x)  # revealed: x
```

a class whose MRO cannot be built has no MRO to search, so its parameters take nothing:

```by
class A:
    def f(self, a: int): ...

class B(A): ...

class C(A, B):  # error: [inconsistent-mro]
    def f(self, a):
        reveal_type(a)  # revealed: a
```

## a chain of overrides

an override declares the type it inherited, so the next one down inherits it in turn:

```by
class A:
    def f(self, a: int): ...

class B(A):
    override def f(self, a): ...

class C(B):
    override def f(self, a):
        reveal_type(a)  # revealed: int
```

## a base parameter that is unannotated itself

a base parameter with no type of its own has nothing to pass on. its anonymous type parameter
belongs to the base method, so the override's parameter opens one of its own:

```by
class A:
    def f(self, a): ...

class B(A):
    override def f(self, a):
        reveal_type(a)  # revealed: a
```

## a generic base

the overridden method is read as the class sees it, so a base specialized in the class header passes
its type arguments down:

```by
class A[T]:
    def f(self, a: T, b: list[T]): ...

class B(A[int]):
    override def f(self, a, b):
        reveal_type(a)  # revealed: int
        reveal_type(b)  # revealed: list[int]

class C[U](A[U]):
    override def f(self, a, b):
        reveal_type(a)  # revealed: U
```

## a type the overridden method binds itself

a type variable of the overridden method, `Self` among them, is bound to that method. copying it
would silently rebind it to the override, so a parameter whose type mentions one takes nothing, and
the others still take theirs:

```by
from typing import Self

class A:
    def f[T](self, a: T, b: int): ...
    def g(self, other: Self): ...

class B(A):
    override def f(self, a, b):
        reveal_type(a)  # revealed: a
        reveal_type(b)  # revealed: int
    override def g(self, other):
        reveal_type(other)  # revealed: other
```

## an overloaded base

an overloaded method accepts the calls of each of its overloads, so the override's parameter takes
the union of what they declare for it:

```by
class A:
    def f(self, a: int) -> int
    def f(self, a: str) -> str
    def f(self, a):
        return a

class B(A):
    override def f(self, a):
        reveal_type(a)  # revealed: int | str
        return a
```

## a constructor

a subclass's constructor takes what that subclass needs, and is not held to the signature of the one
it overrides, so it inherits nothing:

```by
class A:
    def __init__(self, a: int): ...

class B(A):
    def __init__(self, a):
        reveal_type(a)  # revealed: a
```
