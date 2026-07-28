# basedpython: reified type parameters

A PEP 695 type parameter is *reified* when the function body references it in a value position —
anywhere other than a type annotation. The reference becomes a real runtime value (the supplied type
argument), so it types as `type[T]` rather than as the `TypeVar` object. Reification makes the
`[...]` specialization step required — written explicitly, or inferred from the arguments and
injected by the transpiler.

## a value-position type parameter is `type[T]`

```by
def f[T]():
    reveal_type(T)  # revealed: type[T@f]

f[int]()
```

## `isinstance` against a reified parameter

`isinstance`'s second argument is a value position, so `T` reifies and is accepted as a class:

```by
def f[T](t: object) -> bool:
    return isinstance(t, T)

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
    return isinstance(t, T)

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
    reveal_type(T)  # revealed: type[T@f]

f()       # ok — default supplies the reified value
f[str]()  # ok
```

## a missing default is still required when another defaults

Only a trailing run of defaulted parameters may be omitted; a non-defaulted reified parameter still
demands specialization:

```by
def f[T, U = str]():
    reveal_type(T)  # revealed: type[T@f]
    reveal_type(U)  # revealed: type[U@f]

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
    reveal_type(T)  # revealed: type[T@f]
    reveal_type(Args)  # revealed: (*: type)
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

## an unfilled variadic is the empty run

A variadic never forces the specialization step the way a plain reified parameter does: supplying it
nothing is a complete answer, not a missing one, so a bare call stays legal and binds the empty run.
Inference does not solve a variadic from the arguments, so a non-empty run has to be written out:

```by
def f[*Ts](*args: *Ts) -> None:
    assert Ts == () or Ts == (int, str)

f(1, "a")  # Ts is (), not (int, str)
f[int, str](1, "a")
```

## a keyword-variadic pack reifies to its fields

A `**Kwargs` pack is an ordered mapping of field name to type, so its runtime value is that mapping.
Python's subscript grammar takes no keywords, so a keyword specialization lowers to the wrapper's
`__getitem__` call:

```by
def f[**Kwargs]() -> None:
    reveal_type(Kwargs)  # revealed: dict[str, type]
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
