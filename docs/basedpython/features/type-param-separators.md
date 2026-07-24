# type parameter separators

a type parameter list is written the way a value parameter list is, so the same `/` and bare `*`
separators divide it into positional-only, positional-or-keyword, and keyword-only sections:

```by
class C[A, /, B, *, D]: ...
```

`A` can only be given positionally, `D` only by name, and `B` either way. this mirrors how
`def f(a, /, b, *, d)` restricts its value arguments.

## enforcement

the restriction is checked at every specialization site — a subscript on the class, or an
annotation naming it:

```by
class C[A, /, B]: ...

C[A=int, B=str]   # error: `A` is positional-only
C[int, str]       # ok
```

```by
class C[A, *, B]: ...

C[int, str]       # error: `B` is keyword-only
C[int, B=str]     # ok
```

## a variadic is not a separator

a `*Ts` [type variable tuple](generics.md) is a parameter, not a separator. unlike `*args`, it does
not make what follows it keyword-only, because a type argument list resolves a variadic from both
ends:

```by
class C[A, *Ts, B]: ...

C[int, str, bytes, bool]   # A=int, Ts=(str, bytes), B=bool
```

## rules

the separators follow the value-parameter rules:

- at least one parameter must precede `/`
- at least one parameter must follow `*`
- `/` must come before `*`
- neither separator may appear twice

a violation is a syntax error.

## scope

separators are a `.by` reading only. python's own type-parameter grammar has no separators, so a
`/` in a `.py` type parameter list is a syntax error.

## lowering

python type parameter lists cannot carry separators, so they are erased. the positional-only /
keyword-only meaning is checked against the `.by` source, not the emitted python:

```by
class C[A, /, B, *, D]: ...
```

transpiles to:

```python
class C[A, B, D]: ...
```

## see also

- [keyword arguments in subscripts](kw-subscript.md) — binding a type parameter by name
- [generics](generics.md) — the type parameter forms
