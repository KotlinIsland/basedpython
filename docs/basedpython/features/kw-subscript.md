# keyword arguments in subscripts

basedpython allows keyword arguments inside subscriptions:

```by
result = x[a, z=1]
```

transpiles to:

```python
result = x.__getitem__(a, z=1)
```

python's subscript grammar doesn't accept keyword args (PEP 637 was rejected),
so basedpython lowers the call to the explicit `__getitem__` method. positional
and keyword args are forwarded in source order

## type subscripts

when the value is a known generic class, kw subscripts lower to a positional
type subscript instead of a `__getitem__` call. unbound typevars fall back to
their declared defaults:

```by
class A[T = int, R = str]: ...

a: A[T=bool]    # → a: A[bool, str]
b: A[R=int]     # → b: A[int, int]
c: A[R=int, T=bool]    # → c: A[bool, int]
```

ty's type-checking sees the reordered positional form, so type errors point at
the declared typevar order

## single-arg form

`A[T=int]` (no surrounding tuple) is also accepted for single- and multi-typevar
classes. For multi-typevar classes the same defaults rule applies; for
single-typevar classes the keyword name is dropped:

```by
class B[T]: ...
b: B[T=int]     # → b: B[int]
```

## scope

the rewrite fires for any subscription containing at least one keyword binding.
all-positional subscripts are untouched

## see also

- [type parameter separators](type-param-separators.md) — `/` and `*` restrict which type
    parameters may be given by name
- [tuple member access (`expr.N`)](tuple-index.md) — dot-indexing companion form

## inlay hints

a positional type argument of a generic with more than one type parameter gets
that parameter's name as an inlay hint, in the keyword form the subscript would
take:

```by
class Cache[Key, Value]: ...

def f(c: Cache[⟨Key=⟩str, ⟨Value=⟩int]) -> None: ...
```

a subscript that already binds by keyword is left alone, as is a single-typevar
generic (whose keyword is dropped anyway) and a variadic one
