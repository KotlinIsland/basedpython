# basedpython: `type def` type functions

a `type def` is a function from types to a type. it is applied with `[]` in a type expression, and
the application is evaluated by *executing the body* in a real python interpreter.

an application whose arguments are all known is evaluated (and memoized); one that still mentions a
type parameter stays symbolic until specialization. the body itself is not yet type-checked — see
`docs/basedpython/development/type-def-design.md` for the full design and what remains.

**these tests execute `python3`**, which must be on `PATH`. a type function is only executed when it
is first-party; setting `BY_NO_TYPE_FUNCTIONS` disables execution entirely.

## a type function is applied in a type expression

```by
type def F[X]:
    if X <= int:
        return int
    return str

def f(a: F[bool], b: F[float]):
    reveal_type(a)  # revealed: int
    reveal_type(b)  # revealed: str
```

## `<=` is subtyping, not a value comparison

`bool` is a subtype of `int`, `float` is not — the mro decides, not the numeric tower.

```by
type def IsInt[X]:
    if X <= int:
        return int
    return str

def f(a: IsInt[int], b: IsInt[bool], c: IsInt[str], d: IsInt[float]):
    reveal_type(a)  # revealed: int
    reveal_type(b)  # revealed: int
    reveal_type(c)  # revealed: str
    reveal_type(d)  # revealed: str
```

## a type function can build a union

```by
type def Opt[X]:
    return X | None

def f(a: Opt[int], b: Opt[str]):
    reveal_type(a)  # revealed: int | None
    reveal_type(b)  # revealed: str | None
```

## a literal argument carries its value

```by
type def Big[X]:
    if X.literal is not None and X.literal > 3:
        return str
    return int

def f(a: Big[9], b: Big[1]):
    reveal_type(a)  # revealed: str
    reveal_type(b)  # revealed: int
```

## returning `TypeError` reports the author's message

```by
type def Hashed[X]:
    if X.name == "list":
        return TypeError("a list is not hashable")
    return X

def f(a: Hashed[str]):
    reveal_type(a)  # revealed: str

# error: [invalid-type-form] "a list is not hashable"
def g(a: Hashed[list[int]]):
    reveal_type(a)  # revealed: Unknown
```

## a crash in the body is reported as a failure, not a wrong type

```by
type def Boom[X]:
    raise ValueError("author bug")

# error: [invalid-type-form] "could not be evaluated"
def f(a: Boom[int]):
    reveal_type(a)  # revealed: Unknown
```

## the body may do anything a python function may do

the language does not restrict a type function; the footgun belongs to the author.

```by
type def Imports[X]:
    import json
    import urllib.request

    return int if json.dumps([1]) == "[1]" else str

def f(a: Imports[bool]):
    reveal_type(a)  # revealed: int
```

## the name has no runtime existence

a `type def` is erased when transpiling, so naming one in a value position would emit python that
raises `NameError` — it is rejected, in either spelling.

```by
type def F[X]:
    return int

def f():
    # error: [invalid-type-form] "it can only be applied in a type expression, not used as a value"
    print(F)
    # error: [invalid-type-form] "can only be applied in a type expression"
    t = F[int]
```

## an application that still mentions a type parameter is deferred

`F[T]` cannot run — `T` is unknown until the call is specialized. it stays symbolic and behaves as
the declared return type, then re-runs against the substituted argument.

```by
type def F[X] -> int | str:
    if X <= int:
        return int
    return str

def generic[T](x: T) -> F[T]:
    raise NotImplementedError

def unreduced[T](x: F[T]):
    # unreduced: only the declared return is known
    reveal_type(x)  # revealed: int | str

def specialized():
    # the deferred application re-runs once `T` is substituted
    reveal_type(generic(True))  # revealed: int
    reveal_type(generic(1.5))  # revealed: str
```

## an unannotated type function is `Unknown` while unreduced

the declared return type is the only thing an unreduced application can be, so omitting it makes
generic code using the type function unusable.

```by
type def G[X]:
    return int

def unreduced[T](x: G[T]):
    reveal_type(x)  # revealed: Unknown
```

## a bound is a precondition, checked before the body runs

```by
type def Bounded[X: int]:
    return X

def ok(a: Bounded[bool]):
    reveal_type(a)  # revealed: bool

# error: [invalid-type-form] "argument 1 to `Bounded` is `str`, which is not assignable to its bound `int`"
def bad(a: Bounded[str]):
    reveal_type(a)  # revealed: Unknown
```

## a failed application degrades to the declared return, not `Unknown`

```by
type def Boom[X] -> int:
    raise ValueError("author bug")

# error: [invalid-type-form] "could not be evaluated"
def f(a: Boom[str]):
    reveal_type(a)  # revealed: int
```

## a type function can return one of its own arguments, exactly

the argument comes back by handle rather than by name, so a user-defined class survives — and so
does a specialization that a name could not carry.

```by
class MyClass: ...

type def Id[X]:
    return X

def f(a: Id[MyClass], b: Id[list[int]], c: Id[str]):
    reveal_type(a)  # revealed: MyClass
    reveal_type(b)  # revealed: list[int]
    reveal_type(c)  # revealed: str
```

## the body's own output cannot corrupt the result, and a one-line body works

```by
type def Loud[X]:
    print("this goes to stderr, not into the protocol")
    return int

type def OneLine[X]: return int

def f(a: Loud[str], b: OneLine[str]):
    reveal_type(a)  # revealed: int
    reveal_type(b)  # revealed: int
```

## the wrong number of type arguments is a diagnostic, not a crash

```by
type def Two[X, Y]:
    return X

# error: [invalid-type-form] "`Two` takes 2 type arguments, but 1 was given"
def few(a: Two[int]):
    reveal_type(a)  # revealed: Unknown

# error: [invalid-type-form] "`Two` takes 2 type arguments, but 3 were given"
def many(a: Two[int, str, bytes]):
    reveal_type(a)  # revealed: Unknown
```

## a type function cannot be used as a value

it only has meaning in a type expression: the declaration is erased when transpiling, so a value use
would raise `NameError` at runtime.

```by
type def F[X]:
    return int

def f():
    # error: [invalid-type-form] "can only be applied in a type expression"
    t = F[int]
    reveal_type(t)  # revealed: Unknown
```

## any type form may be returned

a returned value reads the way it would in a type position, so `1` is `Literal[1]` and the explicit
`Literal[...]` is optional. a generic form is rebuilt from its origin and arguments, so a
specialization is exact — including one built out of the type function's own argument.

the body is executed on its own, so anything it names beyond builtins must be imported *inside* it.

```by
type def Bare[X]:
    return 1

type def Explicit[X]:
    from typing import Literal

    return Literal[1]

type def Text[X]:
    return "ok"

type def Several[X]:
    from typing import Literal

    return Literal[1, 2]

type def Listed[X]:
    return list[int]

type def Wrap[X]:
    return list[X]

type def Pair[X]:
    return tuple[X, str]

type def Maybe[X]:
    from typing import Optional

    return Optional[X]

def f(a: Bare[int], b: Explicit[int], c: Text[int], d: Several[int]):
    reveal_type(a)  # revealed: 1
    reveal_type(b)  # revealed: 1
    reveal_type(c)  # revealed: "ok"
    reveal_type(d)  # revealed: 1 | 2

def g(a: Listed[int], b: Wrap[str], c: Pair[int], d: Maybe[int]):
    reveal_type(a)  # revealed: list[int]
    reveal_type(b)  # revealed: list[str]
    reveal_type(c)  # revealed: (int, str)
    reveal_type(d)  # revealed: int | None
```
