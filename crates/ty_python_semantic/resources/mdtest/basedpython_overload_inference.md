# basedpython: inferred overload implementation types

The implementation of an overloaded function can omit annotations on its parameters and return type.
The unannotated parameters are inferred as the union of the corresponding overload parameter types
(matched by name), and the declared return type is inferred as the union of the overload return
types. Return statements in the implementation body are then validated against that inferred
declared return type.

```toml
[environment]
python-version = "3.12"
```

## parameter and return type inferred from overloads

```by
def foo(i: str) -> int
def foo(i: int) -> str
def foo(i):
    if 0.1 > 0.3:
        return None  # error: [invalid-return-type]
    return i
```

## parameter type is the union of corresponding overload parameters

```by
def bar(x: str) -> int
def bar(x: int) -> int
def bar(x):
    reveal_type(x)  # revealed: str | int
    return 0
```

## return type union is enforced

```by
def baz(x: str) -> int
def baz(x: int) -> str
def baz(x):
    return x
```

## explicit impl return annotation overrides inference

When the implementation provides its own return annotation, the explicit annotation wins and
inferred overload-union inheritance does not apply.

```by
def qux(x: int) -> int
def qux(x: str) -> str
def qux(x) -> int | str:
    return x
```

## bidirectional inference uses the inherited return type

The inherited return type is supplied as the type-checking context for `return` expressions, so
container literals are inferred against the expected type. This handles invariant generic returns
like `list[object]`: `return [1]` type-checks because `[1]` is inferred as `list[object]` in that
context, not as `list[int]`.

```by
def lift(x: int) -> list[object]
def lift(x: str) -> list[object]
def lift(x):
    return [1]
```

## stub-shaped impl body is not validated

A `...` or docstring-only body is treated as a placeholder, so we don't fire implicit-return-None
even when the inherited return type doesn't admit `None`.

```by
from typing import overload

@overload
def stub_impl(x: int) -> int: ...
@overload
def stub_impl(x: str) -> str: ...
def stub_impl(x): ...
```

## explicit annotation on some impl parameters, inference on the others

```by
def part(x: int, y: bytes) -> int
def part(x: str, y: bytes) -> str
def part(x, y: bytes):
    reveal_type(x)  # revealed: int | str
    reveal_type(y)  # revealed: bytes
    return x
```

## positional-only overloads match positional-or-keyword impl by name

```by
from typing import overload

@overload
def pos(x: int, /) -> int: ...
@overload
def pos(x: str, /) -> str: ...
def pos(x):
    reveal_type(x)  # revealed: int | str
    return x
```

## variadic `*args` inherits the element-type union

```by
from typing import overload

@overload
def gather(*args: int) -> int: ...
@overload
def gather(*args: str) -> str: ...
def gather(*args):
    return args[0]
```

## keyword-variadic `**kwargs` inherits

```by
from typing import overload

@overload
def kw(**kwargs: int) -> int: ...
@overload
def kw(**kwargs: str) -> str: ...
def kw(**kwargs):
    return next(iter(kwargs.values()))
```

## keyword-only parameters are matched by name, not position

```by
from typing import overload

@overload
def kwonly(*, opt: int) -> int: ...
@overload
def kwonly(*, opt: str) -> str: ...
def kwonly(*, opt):
    reveal_type(opt)  # revealed: int | str
    return opt
```

## kind mismatch between overload and impl prevents inheritance

If an overload declares a name as a regular positional parameter but the impl uses the same name as
`*args`, the kinds don't match so the type is not inherited. The impl `*args` falls back to
`Unknown`.

```by
from typing import overload

@overload
def mismatch(args: int) -> int: ...  # error: [invalid-overload]
@overload
def mismatch(args: str) -> str: ...  # error: [invalid-overload]
def mismatch(*args):
    reveal_type(args)  # revealed: (*: Unknown)
    return 0
```

## parameter names that don't appear in any overload fall back to `Unknown`

```by
from typing import overload

@overload
def renamed(a: int) -> int: ...  # error: [invalid-overload]
@overload
def renamed(b: str) -> str: ...  # error: [invalid-overload]
def renamed(z):
    reveal_type(z)  # revealed: z@renamed
    return 0
```

## method overloads on a class — `self` is preserved, body params are inherited

```by
from typing import overload

class C:
    @overload
    def m(self, x: int) -> int: ...
    @overload
    def m(self, x: str) -> str: ...
    def m(self, x):
        reveal_type(x)  # revealed: int | str
        return x

c = C()
reveal_type(c.m(1))  # revealed: int
reveal_type(c.m("a"))  # revealed: str
```

## return-statement validation against the inherited union

```by
from typing import overload

@overload
def strict(x: int) -> int: ...
@overload
def strict(x: str) -> str: ...
def strict(x):
    if x:
        return 1.5  # error: [invalid-return-type]
    return x
```

## external call sites still resolve via the explicit overload signatures

The implementation's inferred parameter/return types do not change which overload is selected at a
call site — overload resolution still walks the `@overload`-decorated signatures.

```by
from typing import overload

@overload
def pick(x: int) -> int: ...
@overload
def pick(x: str) -> str: ...
def pick(x):
    return x

reveal_type(pick(1))  # revealed: int
reveal_type(pick("a"))  # revealed: str
```

## typevars: PEP-695 generic overloads with independent type parameters

Each overload has its own `T`. When inheriting onto the impl we union the per-overload typevar
instances together. Inside the impl body these read as `Unknown` because the typevars are not bound
at the impl scope.

```by
from typing import overload

@overload
def ident[T](x: T) -> T: ...
@overload
def ident[T](x: list[T]) -> list[T]: ...
def ident(x):
    reveal_type(x)  # revealed: T@ident | list[T@ident]
    return x

def use_ident(n: int, xs: list[int]) -> None:
    reveal_type(ident(n))  # revealed: int
    reveal_type(ident(xs))  # revealed: list[int]
```

## typevars: PEP-695 generics with non-generic auxiliary parameters

When only some parameters are generic, the non-generic ones inherit cleanly and the generic ones
produce a typevar union.

```by
from typing import overload

@overload
def aux[T](x: T, tag: int) -> T: ...
@overload
def aux[T](x: T, tag: str) -> T: ...
def aux(x, tag):
    reveal_type(tag)  # revealed: int | str
    return x
```

## typevars: PEP-695 generic with a bound

```by
from typing import overload

@overload
def bounded[T: int](x: T) -> T: ...
@overload
def bounded[T: str](x: T) -> T: ...
def bounded(x):
    return x

def use_bounded(n: int, s: str) -> None:
    reveal_type(bounded(n))  # revealed: int
    reveal_type(bounded(s))  # revealed: str
```

## typevars: legacy `TypeVar` shared across overloads

A module-level `TypeVar` is bound separately in each overload's generic context, so this behaves
like the PEP-695 case above — the inherited type is a union of per-overload typevar instances rather
than a single shared `T`.

```py
from typing import overload, TypeVar

T = TypeVar("T")

@overload
def shared(x: T) -> T: ...
@overload
def shared(x: list[T]) -> list[T]: ...
def shared(x):
    return x

def use_shared(n: int, xs: list[int]) -> None:
    reveal_type(shared(n))  # revealed: int
    reveal_type(shared(xs))  # revealed: list[int]
```

## typevars: probe the impl body's view of a generic parameter

Inside the impl body, the inherited type for a generic parameter is the union of the per-overload
typevar instances. We cannot solve those typevars without a call site, so they appear as `T@...`.

```by
from typing import overload

@overload
def probe[T](x: T, y: T) -> T: ...
@overload
def probe[T](x: list[T], y: list[T]) -> list[T]: ...
def probe(x, y):
    reveal_type(x)  # revealed: T@probe | list[T@probe]
    reveal_type(y)  # revealed: T@probe | list[T@probe]
    return x
```

## typevars: generic overload with a concrete-typed overload sibling

A generic overload alongside a non-generic overload produces a mixed union in the impl. Note that
overload resolution is order-sensitive, so the non-generic overload should appear first if it should
be preferred when both match. The impl body return is checked against the union of overload return
types (`bytes | T@mixed`), which means a bare `return x` from a `T | bytes` parameter is correctly
flagged when no annotation widens it.

```by
from typing import overload

@overload
def mixed(x: bytes) -> bytes: ...
@overload
def mixed[T](x: T) -> T: ...
def mixed(x):
    return x

def use_mixed(n: int, b: bytes) -> None:
    reveal_type(mixed(n))  # revealed: int
    reveal_type(mixed(b))  # revealed: bytes
```

## typevars: typevar appears only in the return type

```by
from typing import overload

@overload
def make[T](kind: type[T]) -> T: ...
@overload
def make[T](kind: list[type[T]]) -> list[T]: ...
def make(kind):
    reveal_type(kind)  # revealed: type[T@make] | list[type[T@make]]
    raise NotImplementedError

def use_make(tcls: type[int], lst: list[type[int]]) -> None:
    reveal_type(make(tcls))  # revealed: int
    reveal_type(make(lst))  # revealed: list[int]
```

## typevars: constrained typevar (multiple bounds)

```by
from typing import overload, TypeVar

S = TypeVar("S", int, str)

@overload
def constrained(x: S) -> S: ...
@overload
def constrained(x: list[S]) -> list[S]: ...
def constrained(x):
    return x

def use_constrained(n: int, xs: list[str]) -> None:
    reveal_type(constrained(n))  # revealed: int
    reveal_type(constrained(xs))  # revealed: list[str]
```

## holes: parameter with a default value on the impl only

The impl supplies a default; the overloads don't. The inherited param type covers the default
value's type so the default is folded into it without widening.

```by
from typing import overload

@overload
def defaulted(x: int) -> int: ...
@overload
def defaulted(x: str) -> str: ...
def defaulted(x=0):
    reveal_type(x)  # revealed: int | str
    return x
```

## holes: default value falls outside the inherited param type

If the default doesn't fit the inherited union, it's unioned in so the body still sees the actual
runtime type.

```by
from typing import overload

@overload
def odd(x: str) -> str: ...
@overload
def odd(x: bytes) -> bytes: ...
def odd(x=0):
    reveal_type(x)  # revealed: str | bytes | 0
    return x  # error: [invalid-return-type]
```

## holes: async overload impl returns a coroutine

```by
from typing import overload

@overload
async def aov(x: int) -> int: ...
@overload
async def aov(x: str) -> str: ...
async def aov(x):
    return x
```

## holes: `@classmethod` overloads — `cls` is preserved

```by
from typing import overload

class K:
    @overload
    @classmethod
    def f(cls, x: int) -> int: ...
    @overload
    @classmethod
    def f(cls, x: str) -> str: ...
    @classmethod
    def f(cls, x):
        reveal_type(x)  # revealed: int | str
        return x

def use_K(n: int, s: str) -> None:
    reveal_type(K.f(n))  # revealed: int
    reveal_type(K.f(s))  # revealed: str
```

## holes: `@staticmethod` overloads

```by
from typing import overload

class S:
    @overload
    @staticmethod
    def g(x: int) -> int: ...
    @overload
    @staticmethod
    def g(x: str) -> str: ...
    @staticmethod
    def g(x):
        reveal_type(x)  # revealed: int | str
        return x
```

## holes: overload chain interrupted by an unrelated definition

If a `def f` appears between overload groups, the overload chain breaks at that point. The later
impl only inherits from overloads that share its contiguous chain.

```by
from typing import overload

@overload
def chain(x: int) -> int: ...
def chain(x):  # first impl — terminates the chain
    return x

@overload
def chain(x: str) -> str: ...
@overload
def chain(x: bytes) -> bytes: ...
def chain(x):
    reveal_type(x)  # revealed: str | bytes
    return x
```
