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
and [`local`](local-lifetimes.md), both are compile-time-only — the keyword is
erased in the lowered python and nothing about it survives to runtime

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
type, a [type alias](../development/how-transpilation-works.md) value, and nested
inside a subscript

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

both keywords are erased:

| basedpython            | python         |
| ---------------------- | -------------- |
| `a: literal str`       | `a: str`       |
| `b: final int`         | `b: int`       |
| `c: list[literal str]` | `c: list[str]` |

nothing in the lowered python distinguishes a restricted annotation from a plain
one, so the markers do not survive a round-trip — the
[reverse transform](../development/reverse-transforms.md) has no lowered shape to
detect, the same as any other erase-only marker. in particular `literal str` does
**not** lower to `LiteralString`: the modifier is a basedpython-level check, and
the lowered python states the plain type

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
