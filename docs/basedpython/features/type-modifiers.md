# use-site type modifiers

`literal` and `final` are keywords written in front of a type expression. they
narrow which values the annotated place accepts, without changing what the value
*is*:

```by
a: literal str = "asdf"   # ok — a string literal
b: final int = 1          # ok — exactly an `int`
b = True                  # error: `bool` is a *sub*class of `int`
```

they are the use-site counterpart to the declaration-site
[modifiers](modifiers.md): `@final` on a class says "nobody may subclass me",
`final T` at a use site says "only exactly a `T` fits *here*". like `abstract`
and [`local`](local-lifetimes.md), they are compile-time-only markers — with one
exception: `literal str` is exactly `LiteralString`, so it lowers to that stdlib
name rather than being erased (see [lowering](#lowering))

`final T` is **not** `typing.Final`. `Final` says a *name* may not be rebound;
`final T` says a *value* must be exactly a `T`. basedpython spells the former
[`let`](modifiers.md)

## `literal T`

`literal T` accepts a value whose type is a **literal type** — one whose values
can only be written down literally in source:

```by
a: literal str = "asdf"
b: literal int = 1
c: literal bool = True
d: literal bytes = b"x"

def s() -> str: ...
e: literal str = s()   # error: a `str` need not be a literal
```

`literal str` denotes exactly the set `typing.LiteralString` does, so it *is*
`LiteralString` — the two spellings are interchangeable and the checker reports
the stdlib name:

```by
def f(x: literal str) -> LiteralString:
    return x    # ok — the same type
```

the other literal value types (`int`, `bool`, `bytes`, `float`, `complex`, and
[enum](enums.md) members) have no stdlib spelling of "any literal of this class",
which is what `literal T` gives them

a constant-folded expression is still a literal, since its type is:

```by
a: literal int = 1 + 1   # ok — `2`
```

### containers

a specialized generic is literal when **every type argument** is. `Never` is
vacuously literal — there is no value to write — so `list[Never]`, the type `[]`
infers, is literal, and its only inhabitant is the empty list display:

```by
a: literal list[*] = []      # ok
b: literal list[*] = [1]     # error: `list[int]` is not literal
```

this is the rule the doc's opening promise rests on: `[]` fits `literal list[*]`
and no other `list[int]` does

## `final T`

`final T` accepts a value whose **runtime class is exactly** `T`'s. a proper
subtype is rejected:

```by
a: final int = 1
b: final int = True    # error: `bool` is a subclass of `int`

class A: ...
class B(A): ...

c: final A = A()       # ok
d: final A = B()       # error
```

a literal is promoted to the class it is an instance of before the comparison,
which is why `1` fits `final int` (its class is `int`) and `True` does not (its
class is `bool`)

on a class already marked [`@final`](modifiers.md) the modifier adds nothing —
such a class has no subclasses — so it reduces to the bare type

### a constructor call is inferred `final`

a call that *names* the class it builds produces a value whose runtime class is
exactly that class, so it is inferred as `final A` rather than `A`:

```by
class A: ...

a = A()                # final A
reveal_type(a)         # final A

def f(make: type[A]):
    make()             # A — the variable may hold a subclass
```

this is the constructor counterpart of literal inference. `1` is inferred as
`Literal[1]` and widens to `int` wherever a declaration is inferred from it, and
`final A` widens to `A` in exactly the same places:

```by
class A: ...
class B(A): ...

class C:
    x = A()            # declared `A`, so a subclass still assigns

def g(c: C):
    c.x = B()          # ok

xs = [A()]             # list[A], not list[final A]
```

what the extra precision buys is disjointness. a value whose class is exactly
`A`'s cannot also be a `str`, and cannot be a *subclass* of `A` either, so both
narrow away — which is what lets a
[`non-overlapping-type-test`](identity-swap.md#a-test-that-can-never-hold) catch
a test that can never hold:

```by
class A: ...
class B(A): ...

def f():
    a = A()
    if a is str: ...   # warning: `final A` and `str` are non-overlapping
    if a is B: ...      # warning: an exactly-`A` value is never a `B`
```

## precedence

a modifier binds to the type expression it precedes, and no further. so it is
tighter than `|`:

```by
a: literal str | None = None   # (literal str) | None — the `None` arm is unrestricted
a = "x"                        # ok
```

but it covers a subscript or a dotted name whole:

```by
b: literal list[str]
c: final mod.Widget
```

## where it may be written

anywhere a type expression appears: a variable annotation, a parameter, a return
type, a [type alias](../development/how-transpilation-works.md) value, a type
parameter's bound or default, a [`cast`](cast.md) target, an
[inline protocol](inline-protocol.md) member, an
[anonymous named tuple](anonymous-named-tuple.md) field, and nested inside a
subscript, a `Callable[[…], R]` parameter list or a parenthesis

each member of a [type mapping](type-mappings.md) is its own type position, so
a modifier may be written inside one: `T in (literal str, literal int)`

the keyword is only read as a modifier when a **name** follows it, exactly as for
the [use-site variance keywords](variance.md). two adjacent names are never valid
python, so `literal str` is unambiguous — while `literal`, `final[int]`,
`literal.Alias` and `final(x)` stay ordinary references to a variable of that
name, and a class really called `literal` keeps working:

```by
class literal: ...
def f(a: literal): ...   # the class, not a modifier
```

the consequence is that a type which does not *start* with a name cannot carry a
bare modifier — a parenthesized [callable type](callable.md), a string forward
reference, a starred type. name the type and the modifier applies as usual:

```by
type Fn = (int) -> None
a: final Fn        # ok — `Fn` is a name
```

## in source position

the restriction applies only where a value is *written into* the annotated place.
reading it back gives an ordinary value of the type the modifier wraps, with all
of its members:

```by
def f(a: literal str, b: final int) -> None:
    a.upper()        # ok — a `str`
    b + 1            # ok — an `int`
    c: str = a       # ok — a `literal str` is a `str`
    d: int = b
```

so `literal`/`final` never make a value *harder* to use; they only constrain what
may be put in

## lowering

`literal str` is the one modifier python can spell, so it lowers to
`LiteralString` — keeping the literal-ness readable by whatever checks the
produced python. every other modifier is erased:

| basedpython            | python                   |
| ---------------------- | ------------------------ |
| `a: literal str`       | `a: LiteralString`       |
| `b: list[literal str]` | `b: list[LiteralString]` |
| `c: literal int`       | `c: int`                 |
| `d: final int`         | `d: int`                 |
| `e: final str`         | `e: str`                 |

the `LiteralString` import is added once per module and [polyfilled](polyfills.md)
to `typing_extensions` below 3.11

because that lowering is faithful, it round-trips: the
[reverse transform](../development/reverse-transforms.md) rewrites `LiteralString`
back to `literal str`, which is also how existing python is converted to the
keyword form:

```sh
by transpile --reverse app.py
```

the reverse rewrite fires only on a name that is spelled `LiteralString` *and*
resolves to it, so an alias (`MyStr = LiteralString`) keeps its own spelling and a
shadowed binding is left alone. once the last reference is rewritten the
`from typing import LiteralString` line is dead and gets pruned

the erased modifiers do **not** round-trip — nothing in the lowered python
distinguishes `literal int` from a plain `int`, so the reverse transform has no
shape to detect, the same as any other erase-only marker

## limits

- `literal` reads the *type* of the value, not the expression that produced it.
    that is usually what you want — a name that still narrows to its literal
    passes — but a value whose type has widened does not:

    ```by
    def f(s: str):
        a: literal str = s   # error — `s` is only known to be a `str`
    ```

- `final` is about a value's *class*, so it only says something interesting about
    a class type. applied to a type with no class behind it — a
    [callable type](callable.md), a [protocol](inline-protocol.md) — it degenerates
    to plain type equality, which an inferred callable rarely satisfies

- the restriction is not tracked through a generic solve: a typevar inferred from
    a `literal T` argument is solved to the literal type itself, not to a
    restricted type

- a gradual type is admissible against both modifiers, as it is against every
    other relation in the type system
