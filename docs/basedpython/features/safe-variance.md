# safe variance

a [private](modifiers.md) member is invisible to external observers, so it
cannot break variance — which lets a [covariant](variance.md) `out T` class hold
a mutable `T`-typed field:

```by
class A[out T]:
    private t: T      # mutable field under `out T` — sound because it is private

    def f(self, other: A[object]):
        other.t = 1   # error — privacy violation
```

a [covariant](variance.md) type parameter (`out T`) is only sound while `T`
is never written through, nor read off of, a *widened* view of the class.
the classic hole is a mutable `T`-typed field: `A[int]` is assignable to
`A[object]` under covariance, and if the `A[object]` reference can write its
field then the underlying `int` storage gets corrupted with a `str`

mainstream checkers close this hole by forbidding `out T` from appearing in
any mutable or input position — which rules out a great deal of otherwise
reasonable code. basedpython closes it with a narrower observation:
**privacy is what makes variance safe**. a [private](modifiers.md) member is
invisible to external observers, so it cannot be used to distinguish two
specializations of the same class, so it cannot break variance. the type
checker leans on this in three places

## private members do not specialize through a widened view

a private member is only accessible through a `self` whose type parameter is
exactly the enclosing class's own — never through a foreign or widened
specialization of the same class. that single rule is what permits a mutable
`T`-typed field under `out T`:

```by
class A[out T]:
    private t: T

    def f(self, other: A[object]):
        print(other.t)   # error — privacy violation
        other.t = 1      # error — privacy violation

    def g(self: A[object]):
        self.t = "asdf"  # error — privacy violation
```

`other: A[object]` is a *different* specialization than the `A[T]` whose body
we are inside, so its private `t` is off-limits even from within class `A`.
`self: A[object]` is the same offence written on the receiver: an explicitly
widened `self` is no longer the precise `A[T]`, so its `t` is unreachable too

the only place `t` *is* reachable is through a `self` carrying the exact `T`.
there the field's declared type and the receiver's type argument coincide, so
both reads and writes are sound. covariance never gets a chance to corrupt the
field because the only reference that can touch it is the one that already
knows the real `T`

without this rule the field would have to be invariant (`A[int]` not
assignable to `A[object]`), losing the covariance the author asked for. the
privacy boundary buys back the assignability while keeping it sound — the
mutable field simply does not exist as far as a widened observer is concerned

## `SafeVariance[T]` — consuming `T` at its upper bound

sometimes a covariant class genuinely needs a method that *takes* a `T`.
ordinarily `out T` may not appear in an input position at all. `SafeVariance`
is the escape hatch: a parameter typed `SafeVariance[T]` is checked against the
actual specialization at the **call site**, but is seen as the **upper bound of
`T`** inside the body:

```by
class A[out T]:
    private t: T

    def f(self, t: SafeVariance[T]):
        reveal_type(t)   # object — the upper bound of T
        self.t = t       # error — object is not assignable to T

a = A[int]()
a.f("asdf")   # error — call site checks against the real T (int)
a.f(1)        # ok
```

the two halves pull in opposite directions on purpose:

- at the call site, `SafeVariance[T]` behaves like `T`, so the specialization
    is enforced — `a: A[int]` accepts `f(1)` and rejects `f("asdf")`. callers
    cannot smuggle an arbitrary value in
- inside the body, `t` is widened to `T`'s upper bound (`object` here), so the
    body learns nothing more than the bound. crucially it cannot store the value
    back into the covariant field: `self.t` wants a `T`, the body only has an
    `object`, and `object` is not assignable to `T`

that second half is the soundness guard. the body can read the consumed value,
log it, compare it — anything that treats it as its bound — but it can never
funnel it into `T`-typed covariant storage, which is the only operation that
could violate covariance. the value flows *in* at full precision and is
immediately *erased* to the bound, so it can never flow back out mislabelled

> the `SafeVariance[T]` surface annotation is reserved for a future release.
> it is documented here as the intended spelling; until the annotation syntax
> lands, the mechanism above is the specified behaviour, not yet a usable form

## private members are bivariant, not covariant

variance inference normally treats a mutable `T`-typed attribute as forcing
`T` **invariant** (it is both read and written), and a read-only one as
covariant. a *private* attribute contributes neither constraint, because no
external observer can tell two specializations apart through it. with no
constraint from any externally-visible position, the inference defaults to
**bivariant**:

```by
class A[T]:
    _t: T   # private — does not constrain variance
```

here `T` is inferred bivariant: both `A[int] -> A[object]` and
`A[object] -> A[int]` are allowed, because there is no observable use of `T`
anywhere on the public surface to distinguish them. a single-underscore name
is private, so `_t` is exactly the case the first section made sound — and a
field that cannot break variance cannot constrain it either

this is strictly more permissive than the covariant guess a naive reading of
"`T` is only read" would produce, and it is sound for the same reason: the
field is invisible from outside, so neither direction of assignment can be
caught misbehaving. the moment any public member mentions `T`, that member's
position drives the inference in the usual way and the bivariance disappears

privacy is read off the member's name: a leading underscore is private, and so
is a name-mangled `__t`. a dunder is *not* private — it is part of the public
protocol surface — so `__t__: T` keeps constraining variance. a private
*method* counts too: `def _consume(self, t: T)` leaves `T` bivariant

> a class-body `private` member keyword is currently stripped without renaming,
> so it does not by itself mark the member private to the type checker. write the
> underscore name until that lands

the behaviour is controlled by `analysis.bivariant-private-attributes`, which is
enabled by default. set it to `false` to fall back to treating a private
attribute as immutable-but-readable, which constrains `T` to covariance

```toml
[analysis]
bivariant-private-attributes = false
```

the option is resolved per module, so the module that *declares* a class governs
how that class's variance is inferred, no matter which module reads it

## why privacy is the common thread

all three behaviours are one principle applied three ways. a private member is
not part of the type's observable interface, so:

1. it may be a mutable field under `out T`, because only the precisely-typed
    `self` can reach it (section 1)
1. it may be the sink for a `SafeVariance[T]` parameter's value — except the
    body is handed only the bound, so the sink stays unreachable in practice
    (section 2)
1. it imposes no variance constraint at all, leaving `T` bivariant when nothing
    public mentions it (section 3)

soundness comes from the same fact each time: you cannot observe, through a
widened reference, a difference that a private member would have introduced
