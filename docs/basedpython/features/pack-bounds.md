# bounds on a variadic pack

a variadic type parameter may carry an upper bound, and the star count on the bound decides what
it constrains. an *unstarred* bound bounds every member of the pack; a *starred* one — taking the
same star count the pack was declared with — bounds the pack as a whole:

```by
class EveryElement[*Ts: int]: ...

class WholeTuple[*Ts: *(int, str)]: ...

class EveryField[**Kwargs: int]: ...

class WholeShape[**Kwargs: **{"a": int}]: ...
```

this mirrors the value-parameter forms exactly, the way the rest of the
[type parameter list](generics.md) does: `*args: int` types each argument while `*args: *Ts`
types the whole run, and `**kwargs: int` types each value while `**kwargs: **Kwargs` types the
whole mapping

CPython rejects a bound on a `TypeVarTuple` and on a `ParamSpec`, so all four forms are `.by`-only

## element-wise

`*Ts: int` requires every element of the pack to be a subtype of `int`:

```by
class A[*Ts: int]: ...

def ok(a: A[int, bool]): ...
def bad(a: A[int, str]): ...  # error
```

`**Kwargs: int` is the same for a [keyword-variadic pack](keyword-variadic.md) — every *field's
type* is bounded, and the field names are unconstrained:

```by
class A[**Kwargs: int]: ...

def ok(a: A[x=int, y=bool]): ...
def bad(a: A[x=int, y=str]): ...  # error
```

## whole-pack

`*Ts: *X` is an ordinary assignability check against the packed tuple, so `X` constrains the
pack's length as well as its elements:

```by
class A[*Ts: *(int, str)]: ...

def ok(a: A[int, str]): ...
def narrower(a: A[bool, str]): ...  # a tuple is covariant in its elements
def too_short(a: A[int]): ...  # error
def wrong_element(a: A[int, bytes]): ...  # error
```

a variable-length bound admits any length:

```by
class A[*Ts: *tuple[int, ...]]: ...

def one(a: A[int]): ...
def many(a: A[int, bool, int]): ...
```

`**Kwargs: **X` names the fields the pack must have. `X` is a
[dict literal type](typed-dict-literal.md) or a `TypedDict`; every field it names has to be
present with an assignable type. extra fields are what an upper bound permits:

```by
class A[**Kwargs: **{"a": int}]: ...

def ok(a: A[a=int]): ...
def extra(a: A[a=int, b=str]): ...
def wrong_type(a: A[a=str]): ...  # error
def missing(a: A[b=str]): ...  # error
```

## inferred packs

a pack solved from a call is checked where it is solved, not only where it is written out:

```by
class A[**Kwargs: int]:
    init(**kwargs: **Kwargs)


a = A(x=1, y=True)  # A[x=int, y=bool]
b = A(x=1, y="s")  # error
```

## a pack bound is not a type bound

an ordinary type parameter's bound is an upper bound on the type it stands for, so an
unspecialized `T` behaves like its bound — `T: int` supports `+`, and `T` is assignable to `int`.
a pack's bound is not that: the pack's value is a tuple or a field mapping, and the bound
describes its members or its shape. an unspecialized `*Ts` or `**Kwargs` therefore behaves exactly
as it does without a bound, and the bound is checked only where the pack is specialized

## lowering

python has no bound on either kind of pack, so the bound is erased:

```by
class A[*Shape: *(int, str), **Kwargs: **{"a": int}]: ...
```

transpiles to:

```python
class A[*Shape, **Kwargs]: ...
```

the bound is checked against the `.by` source, not the emitted python — the same way
[bound ranges](bound-ranges.md) and [constraints](constraints.md) are

## see also

- [generics](generics.md) — the type parameter forms
- [keyword-variadic packs](keyword-variadic.md) — what `**Kwargs` declares
- [dict literal types](typed-dict-literal.md) — the `{"a": int}` bound spelling
- [type parameter bound ranges](bound-ranges.md) — `T: Lower..Upper` on an ordinary type parameter
- [explicit typevar constraints](constraints.md) — `T: constraints (int, str)`
