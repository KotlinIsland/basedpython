# safe variance

a [private](modifiers.md) member is invisible to external observers, so it
cannot break variance — which lets a [covariant](variance.md) `out T` class hold
a mutable `T`-typed field:

```by
class A[out T]:
    private t: T      # mutable field under `out T` — sound because it is private

    def f(self, other: A[object]):
        other.t = 1   # error: `int` is not a `T`
```

a [covariant](variance.md) type parameter (`out T`) is only sound while `T`
is never written through, nor read off of, a *widened* view of the class.
the classic hole is a mutable `T`-typed field: `A[int]` is assignable to
`A[object]` under covariance, and if the `A[object]` reference can write its
field then the underlying `int` storage gets corrupted with a `str`

mainstream checkers close this hole by forbidding `out T` from appearing in
any mutable or input position, private ones included — which rules out a great
deal of otherwise reasonable code. basedpython keeps [the
rule](variance.md#the-declaration-has-to-match-the-usage) for the class's public
surface, and narrows it with one observation:
**privacy is what makes variance safe**. a [private](modifiers.md) member is
invisible to external observers, so it cannot be used to distinguish two
specializations of the same class, so it cannot break variance. the type
checker leans on this in three places

## private members do not specialize

a private member is invisible outside its class, so a *widened* view of the
class learns nothing about it. that is the whole rule: through any receiver but
the class's own, a private member keeps its declared type instead of picking up
the receiver's type arguments. a write has to supply a real `T`, and a read gets
back only what such a view actually knows — `T`'s bound

```by
class A[out T]:
    private t: T

    # the public `t: T` parameter is itself reported — `out T` may not be consumed
    # in public (see [variance](variance.md)); it is written that way here only to
    # name a real `T` inside the body
    def f(self, other: A[object], t: T):
        reveal_type(self.t)    # T@A — `self` carries `A`'s own parameter
        reveal_type(other.t)   # object — a widened view knows only the bound

        other.t = t            # ok — a real `T`
        other.t = 1            # error: `int` is not a `T`
        self.t = other.t       # error: `object` is not a `T`
```

nothing here needs its own diagnostic. the member's type simply never picks up
`object`, so an ordinary assignability check does the work, and the erased read
cannot be funnelled back into `T`-typed storage — which is the only operation
that could corrupt the field

an explicitly widened `self` is a widened view like any other, but it is still a
receiver you may write a `T` to:

```by
class A[out T]:
    private t: T

    def f(self: A[object], t: T):   # the `t: T` parameter is reported, as above
        self.t = t             # ok — `t` is a real `T`
        self.t = "asdf"        # error: `str` is not a `T`
```

and so is a construction: inside `A`'s own body `A()` is an `A[Unknown]`, not
the `A[T]` the body is written against

```by
class A[T]:
    private t: T

    def f(self):
        A().t = 1              # error: `int` is not a `T`
```

without this rule the field would have to be invariant (`A[int]` not
assignable to `A[object]`), losing the covariance the author asked for. the
privacy boundary buys back the assignability while keeping it sound — the
mutable field simply does not exist as far as a widened observer is concerned

### the erased read is what keeps the other directions honest

covariance is not the only way a specialization can be widened. under `in T` the
widening runs the other way — `A[object]` is an `A[int]` — so a *read* is the
unsound direction, and erasing it is what catches that:

```by
class A[in T]:
    private t: T

    def f(self):
        a1 = A[object]()
        a2: A[int] = a1        # fine — `A` is contravariant
        print(a2.t + 1)        # error: `object` has no `__add__`
```

`a2` really holds an `A[object]`, so its `t` is not an `int` whatever `a2`'s
type says. reading it as `object` is the only answer that is not a lie. the same
erasure applies to a private *method*: a `private def consume(self, t: T)`
reached through a widened view takes a `Never`, so nothing can be passed to it,
while a `private def produce(self) -> T` still returns the bound

the erasure is the type's top materialization, not a naive swap of the bound, so
it stays sound where the parameter is nested in an invariant position:

```by
class A[T]:
    private items: list[T]

def f(a: A[int]):
    reveal_type(a.items)       # list[*]
```

### an invariant type parameter specializes as usual

the rule exists to close a widening hole, so it only applies where one exists.
if the type parameter the member's type names is **invariant**, no
specialization of the class is assignable to any other, every receiver's type
argument is exact, and ordinary specialization is already sound:

```by
class A[T]:
    private t: T

    # a public member that both reads and writes `T` forces invariance
    def swap(self, t: T) -> T:
        old = self.t
        self.t = t
        return old

    def f(self):
        a1 = A[int]()
        reveal_type(a1.t)      # int
        a1.t = 1               # ok — `A[int]` is not an `A[object]`
```

the same exemption covers a private member whose type never mentions a type
parameter at all (`private n: int`), and a class that is not generic — which is
every `_`-prefixed attribute in ordinary python code

privacy for this rule is read the same way as for [variance
inference](#private-members-are-bivariant-not-covariant): the `private` keyword,
a leading underscore, or a name-mangled `__t`. a dunder is not private

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

privacy is read off the member's name as well as the `private` keyword: a
leading underscore is private, and so is a name-mangled `__t`. a dunder is *not*
private — it is part of the public protocol surface — so `__t__: T` keeps
constraining variance. a private *method* counts too: both
`def _consume(self, t: T)` and `private def consume(self, t: T)` leave `T`
bivariant

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

1. it may be a mutable field under `out T`, because a widened view of the class
    never learns what its type parameter is — the member does not specialize, so
    only a real `T` can be written to it and only the bound can be read back
    (section 1)
1. it may be the sink for a `SafeVariance[T]` parameter's value — except the
    body is handed only the bound, so the sink stays unreachable in practice
    (section 2)
1. it imposes no variance constraint at all, leaving `T` bivariant when nothing
    public mentions it (section 3)

soundness comes from the same fact each time: you cannot observe, through a
widened reference, a difference that a private member would have introduced
