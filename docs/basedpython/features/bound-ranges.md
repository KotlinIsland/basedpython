# type parameter bound ranges

a type parameter bound is written `T: Upper`, which pins only the top of the range — `T` may still
be specialized to anything below `Upper`, including `Never`. a bound range pins both ends:

```by
class C[T: str..object]: ...
```

the left end is the lower bound and the right end is the upper bound, so this reads
`str <: T <: object`. both ends are required; `T: ..object` is spelled `T: object`, and `T: str..`
has no spelling because an upper end is always needed to bound the range. the two dots are written
as one unit — `T: str . . object` is not a range.

## what the lower end buys you

with only an upper bound, nothing is assignable *to* a type parameter — the checker cannot know
that a given `T` is wide enough to hold anything at all:

```by
class C[T: object]:
    def f(self) -> T:
        return "a"   # error: expected `T@C`, found `"a"`
```

a lower bound puts a floor under every specialization, so anything at or below the lower end is
assignable to `T`:

```by
class C[T: str..object]:
    def f(self) -> T:
        return "a"   # ok — `str` is assignable to every valid `T`
```

this is the constraint a default implementation in a generic base class needs: the body is written
against a particular type, and the bound records which specializations it is valid for.

## it is a bound, not an equality

every type at or above the lower end is a valid specialization:

```by
class C[T: str..object]: ...

C[str]         # ok
C[object]      # ok
C[str | int]   # ok
C[int]         # error: `int` does not satisfy lower bound `str`
```

## the upper end is unchanged

`T: Lower..Upper` behaves exactly like `T: Upper` everywhere the upper bound is consulted — member
lookup, narrowing, and specialization checks all see the same upper bound they would have seen
without the range. in particular the lower end says nothing about what a `T` *has*:

```by
class C[T: str..str]: ...

C[object]   # error: `object` is not assignable to upper bound `str`
```

```by
class C[T: str..object]:
    def f(self, x: T) -> int:
        return len(x)   # error: `T@C` is not `Sized` — that is the upper end's job
```

## both ends must accept the default

a default is a specialization like any other:

```by
class C[T: str..object = int]: ...   # error: default `int` is not assignable from lower bound `str`
class D[T: str..str = object]: ...   # error: default `object` is not assignable to upper bound `str`
```

## either end can name a type parameter already in scope

both ends follow the same scope rule as a plain upper bound — see
[generic parameter syntax](generics.md#a-bound-can-name-another-type-parameter):

```by
def g[T, U: T..object](t: T, u: U) -> U:
    return u
```

`Self` is not one of those names: it is bound by the enclosing class rather than by the list being
declared, and binding the receiver settles it. it is a valid lower end:

```by
class C:
    def f[T: Self..object](self) -> T:
        return self
```

## a range needs a plain upper end

a [type mapping](type-mappings.md) is an unordered set rather than the top of a range, so the two
forms are alternatives — `in` and `:` cannot both introduce the same parameter. a parameter list is
not a type either, so it cannot cap a range:

```by
class C[T: int..(*: *, **: *)]: ...   # error: needs a plain upper bound
```

## empty ranges

if the lower end is not assignable to the upper end, no type can satisfy the range and the
declaration is an error:

```by
class C[T: object..str]: ...   # error: lower bound `object` is not assignable to upper bound `str`
```

## composing the ends

each end is an ordinary type expression, so unions and intersections compose the usual way. an
intersection on the upper end narrows it; a union on the lower end widens the floor:

```by
class C[T: str..(Sized and Hashable)]: ...
class D[T: (str or int)..object]: ...
```

## scope

ranges are a `.by` reading only. python's type-parameter grammar has a single bound, so `..` in a
`.py` type parameter list is a syntax error.

## lowering

python bounds have no lower end, so it is erased and only the upper end is emitted:

```by
class C[T: str..object]: ...
```

transpiles to:

```python
class C[T: object]: ...
```

or, when the [`Generic[...]` polyfill](generics.md) applies, to
`_T = TypeVar("_T", bound=object)`. the lower bound is checked against the `.by` source, not the
emitted python.

## see also

- [generics](generics.md) — the type parameter forms
- [bounds on a variadic pack](pack-bounds.md) — what a bound means on a `*Ts` or `**Kwargs`
- [type mappings](type-mappings.md) — `T in (int, str)`, an unordered
    alternative to a range
- [typevar variance keywords](variance.md) — which direction subtyping moves a specialization
