# generic parameter syntax

type parameter lists accept the full set of value parameter markers, so the same
`/`, `*`, `*Args`, `**Kwargs` carry the meaning they have in `def`:

```by
class A[Positional, /, PositionalOrNamed, *, Named]

class B[Positional, /, PositionalOrNamed, *Args, Named, **Kwargs]
```

`*Args` behaves the same as a type variable tuple. `**Kwargs` captures a typed
dictionary of named type arguments

## motivation

parameter specifications do not actually accept variadic arguments, despite the
double star notation in the declaration

type parameter declarations also diverge from value
parameter declarations: there is no positional-only marker, no keyword-only
marker, and `**kwargs` has no analogue at all

basedpython mirrors value parameter syntax 1-to-1 in type parameter lists, so
that the same `/`, `*`, `*Args`, `**Kwargs` markers carry the same meaning
they have in `def`

## syntax

specialization at the call site uses the same positional / keyword form:

```by
A[
    int,                  # Positional
    PositionalOrNamed=str,
    Named=int,
]

B[
    int,                  # Positional
    str,                  # PositionalOrNamed
    int,                  # Args
    str,                  # Args
    Named=int,
    foo=str,              # Kwargs
    bar=int,              # Kwargs
]
```

## parameter specifications

parameter specification is replaced by the new enhanced tuple types, declared as a
bound on a type parameter:

```by
# `*` here means the projected top type, which differs from `*: object, **: object`
def f[P: (*: *, **: *)](fn: (**P) -> None) -> (**P) -> int
```

call-site arguments for any type parameter can use an enhanced tuple type,
representing the inputs for a callable:

```by
f[(int, str)](lambda *_: None)

class A[P = (int, *: str)]

A[(bool, a: str, b: str)]
```

## concatenate replacement

`Concatenate` is replaced with an unpack:

```by
def f[P: (*: *, **: *)](fn: (**P) -> None) -> (int, **P) -> None
```

## forwarding

a parameter specification is forwarded whole, as the pair that takes its positional half
and its keyword half:

```by
def deco[P: (*: *, **: *), R](fn: (**P) -> R) -> (**P) -> R:
    def inner(*args: *P, **kwargs: **P) -> R:
        return fn(*args, **kwargs)

    return inner
```

the star count follows the half being taken, the way it does for every other pack — python's
`P.args` / `P.kwargs` is not valid in a basedpython file

## callable attribute access

`Callable` exposes its parameter list and return type as attributes-as-types:

```by
class Callable[Parameters: (*: *, **: *), Return]:
    @type_check_only
    parameters: Parameters

    @type_check_only
    returns: Return

class A[Fn: (*: *, **: *) -> object]:
    def f(self, *args: *Fn.parameters, **kwargs: **Fn.parameters) -> Fn.returns
```

## see also

- [bounds on a variadic pack](pack-bounds.md) — what a bound means on a `*Args` or `**Kwargs`
- [keyword-variadic packs](keyword-variadic.md) — what `**Kwargs` declares
- [type parameter separators](type-param-separators.md) — the `/` and bare `*` markers
