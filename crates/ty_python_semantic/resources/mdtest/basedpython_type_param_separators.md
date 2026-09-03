# basedpython: type parameter separators

a type parameter list is written the way a value parameter list is, so the same `/` and bare `*`
separators divide it into positional-only, positional-or-keyword, and keyword-only sections. in
`class C[A, /, B, *, D]`, `A` can only be given positionally, `D` only by name, and `B` either way.

```toml
[environment]
python-version = "3.12"
```

## positional-only cannot be given by name

```by
class C[A, /, B]: ...

# error: [invalid-type-arguments] "Type variable `A` is positional-only"
def f(c: C[A=int, B=str]): ...
```

## positional-only is fine positionally

```by
class C[A, /, B]: ...

def f(c: C[int, str]):
    reveal_type(c)  # revealed: C[int, str]
```

## keyword-only cannot be given by position

```by
class C[A, *, B]: ...

# error: [invalid-type-arguments] "Type variable `B` is keyword-only"
def f(c: C[int, str]): ...
```

## keyword-only is fine by name

```by
class C[A, *, B]: ...

def f(c: C[int, B=str]):
    reveal_type(c)  # revealed: C[int, str]
```

## positional-or-keyword accepts either

```by
class C[A, /, B, *, D]: ...

def f(pos: C[int, str, D=bytes], kw: C[int, B=str, D=bytes]):
    reveal_type(pos)  # revealed: C[int, str, bytes]
    reveal_type(kw)  # revealed: C[int, str, bytes]
```

## the user's example

```by
class C[A, /, B, *, D]: ...

# error: [invalid-type-arguments] "Type variable `A` is positional-only"
# error: [invalid-type-arguments] "No type arguments provided for required type variables `B`, `D` of class `C`"
def f(c: C[A=int]): ...
```

## a variadic is not a separator

a `*Ts` type variable tuple does not make what follows it keyword-only — a type argument list
resolves a variadic from both ends, so trailing parameters stay positional

```by
class C[A, *Ts, B]: ...

def f(c: C[int, str, bytes, bool]):
    reveal_type(c)  # revealed: C[int, str, bytes, bool]
```

## a variadic cannot be given by name

a `*Ts` variadic binds an unknown-length run of positions, so it cannot be given by name — the same
way `def f(*args)` rejects `f(args=1)`

```by
class X[*Args]: ...

# error: [invalid-type-arguments] "Type variable `Args` is variadic and cannot be given by name"
def f(x: X[Args=int]): ...
```

## a variadic is fine positionally

```by
class X[*Args]: ...

def f(x: X[int, str]):
    reveal_type(x)  # revealed: X[int, str]
```

## a separated list still checks its bounds

separators send the whole subscript through the by-name pipeline even when every argument is
positional, and a type variable's bound holds there just as it does without them.

```by
class C[A: int, /, B: str]: ...

# error: [invalid-type-arguments] "Type `bytes` is not assignable to upper bound `int` of type variable `A@C`"
# error: [invalid-type-arguments] "Type `int` is not assignable to upper bound `str` of type variable `B@C`"
def f(c: C[bytes, int]): ...
```

## the separators only apply in `.by`

the same class written in a `.py` file has no separators — `/` there is a syntax error, so this is a
plain generic

```py
class C[A, B]: ...

def f(c: C[int, str]):
    reveal_type(c)  # revealed: C[int, str]
```
