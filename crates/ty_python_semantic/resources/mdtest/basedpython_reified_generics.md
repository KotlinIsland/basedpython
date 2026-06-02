# basedpython: reified type parameters

A PEP 695 type parameter is *reified* when the function body references it in a value position —
anywhere other than a type annotation. The reference becomes a real runtime value (the supplied type
argument), so it types as `type[T]` rather than as the `TypeVar` object. Reification makes the
`[...]` specialization step structurally required, on top of the usual call.

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

## specialization is mandatory

Python carries no runtime type information, so a reified type parameter cannot be inferred from the
arguments — it must be supplied explicitly:

```by
def f[T](t: object) -> bool:
    return isinstance(t, T)

f[int](1)  # ok
# error: [unspecialized-reified-generic] "Cannot call reified generic function `f` without explicit specialization"
f(1)
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
