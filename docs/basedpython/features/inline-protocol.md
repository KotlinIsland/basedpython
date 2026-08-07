# inline protocol types

`protocol(...)` in an annotation position is an anonymous structural protocol.
members are separated by `;`:

```by
def render(x: protocol(name: str; def greet(self, who: str) -> str)) -> str:
    return f"{x.name}: {x.greet('world')}"
```

`name: T` declares a data member and `def name(...) -> T` declares a method
member. a method member's first parameter is the receiver, so it binds away on
access — that is what distinguishes it from a data member whose type happens to
be a callable:

```by
a: protocol(def m(self, n: int) -> str)  # a.m is `(n: int) -> str`
b: protocol(m: (int) -> str)             # b.m is `(int, /) -> str`
```

every parameter after the receiver keeps the meaning it has in
[callable arrow syntax](callable.md) — a bare name is a positional-only
parameter's type, `name: T` is a named parameter, and the `/`, `*`, `*args: T`
and `**kwargs: T` forms all work

the first position is always the receiver, so a method with no parameters of its
own still spells it. `def m(int) -> str` declares a receiver *named* `int`, not
a positional-only `int` parameter — write `def m(self, int) -> str` for that

an inline protocol is structural and has no identity of its own, so two
occurrences of the same members are the same type wherever they are written,
and any class with matching members satisfies it without inheriting anything

## a call on a type parameter

a method member binds its receiver away, but it still names *that* receiver's
method, so a call on a type parameter is the [symbolic](symbolic-type-ops.md)
`T.m()` rather than the return type the protocol declares. specializing the
parameter re-resolves the call against whatever it was specialized to:

```by
class B: ...

class X:
    def foo(self) -> B:
        return B()

def f[T: protocol(def foo(self) -> B)](t: T):
    return t.foo()

reveal_type(f(X()))  # B
```

a `Protocol` class bound answers the same way, so the two spellings of an
interface agree

## across several lines

members may be spread over several lines, with an optional trailing `;`:

```by
def f(
    x: protocol(
        a: int;
        def m(self) -> str;
    ),
) -> None: ...
```

## keyword unpacks

a [keyword-variadic pack](keyword-variadic.md) splices its whole field list into
the member list with `**Kwargs`. each field becomes a data member once the pack
is specialized:

```by
class A[**Kwargs]:
    def __init__(self, **kwargs: **Kwargs) -> None: ...
    def get(self) -> protocol(**Kwargs): ...

a = A(foo=1, bar="x")
reveal_type(a.get().foo)  # int
```

a pack composes with members written out longhand — `protocol(tag: int; **Kwargs)`. an unspecialized pack contributes nothing yet and is carried until it
is, so a `protocol(**Kwargs)` parameter accepts any argument while the enclosing
scope is still generic; the requirement materializes at the specialization site

## `protocol` is a soft keyword

`protocol(x)` is still a call to something named `protocol`. only a
parenthesized list whose first member is unambiguously a member declaration —
`def`, or `name: T` — reads as an inline protocol, in any file. a leading
`**Pack` is the exception: `protocol(**kwargs)` is also an ordinary call, so it
only reads as a member list in a `.by` file

## display

an inline protocol reads back as the members it declares:

```by
def f(x: protocol(a: int; def m(self) -> str)):
    reveal_type(x)  # <Protocol with members 'a', 'm'>
```

an unspecialized pack shows as the pending splice — `<Protocol with members **Kwargs@A>`

## lowering

each shape is hoisted to one module-level `Protocol` class. those classes land
ahead of everything the module defines, and a member can name a class declared
later, so member types are emitted as forward references:

```python
class _Protocol_<hash>(Protocol):
    a: "int"
    def m(self) -> "str": ...
```

a `**Kwargs` splice has no members to erase to — they are only known at the
specialization site, and python erases type arguments anyway — so it
contributes nothing to the generated class

a member naming a type variable is rewritten to the mangled name the
[generics](generics.md) polyfill gives it below python 3.12, since the hoisted
class sits outside the scope that declared it
