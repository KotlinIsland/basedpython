# planned language features

five modifiers are in flight that the compiler cares about. none is on `main`
yet, and two of them change the design in [ir](ir.md) and
[optimizations](optimizations.md) rather than merely adding to it — so they are
recorded here before they land, with the representation consequences worked out
and the requests the compiler needs to make back to the language design

| modifier        | kind       | status                                               | codegen impact                             |
| --------------- | ---------- | ---------------------------------------------------- | ------------------------------------------ |
| `final T`       | use site   | implemented, branch `claude/literal-final-modifiers` | large                                      |
| `literal T`     | use site   | implemented, same branch                             | large                                      |
| `single T`      | type param | design sketch, verdict "build it, scope it honestly" | **largest of the five**                    |
| `frozen T`      | type param | design sketch, verdict "do not build as specified"   | none, if redefined; a pessimization if not |
| `overlapping T` | type param | design sketch, verdict "build it"                    | none directly                              |

the two design-sketch verdicts come from the adversarial review in
`features/type-param-modifiers.md`. this document does not relitigate them; it
adds the compiler's evidence, which in two places points the same way the review
already did

## `final T`: exactness at the use site

`final T` accepts a value whose **runtime class is exactly** `T`'s. `final int`
rejects `True`, because `bool` is a subclass of `int`

this is the single most useful thing a type system can tell a compiler about a
place, and basedpython is the only python dialect that can say it *per place*
rather than per class. `final class Widget` forbids subclassing everywhere;
`def draw(w: final Widget)` demands exactness at one parameter and leaves the
class open for everyone else

### what it unlocks

**devirtualization where the class is open.** the receiver's type object is
known, so a method call is a direct call:

```c
Widget_render(w);                        /* final Widget      */
w->ob_type->vt[WIDGET_RENDER](w);        /* Widget            */
```

**layout pinning, which is the precondition `StackAlloc` was missing.** the
[stack allocation](optimizations.md#stack-allocation) pass needs the allocation
size, and an open class gives it only an upper bound of "unknown" — a value
typed `A` may be a `B` with more fields. `final A` pins the layout, so `final`
and `local` together are the complete precondition: `local` proves it does not
escape, `final` proves how big it is. neither is sufficient alone, and the
existing design only had the first

**a semantic delta gets smaller.** [plan](plan.md#semantic-deltas) records that
an `int`-typed register loses the `True`/`1` distinction, because the tagged
representation has no room for it. `final int` rejects `True` at the checker, so
a `final int` place cannot observe the divergence at all. the delta survives, but
it now has a spelling that opts out of it

### the trap: exactness is checked, not proven

`restriction_admits` returns `true` for `Dynamic`, `Divergent`, and `Never`
before it looks at the modifier — a gradual value is admissible against `final T`
exactly as it is against every other relation in the type system. so a
`final Widget` parameter can receive an `Any` with no diagnostic, and **the
compiler may not treat `final` as a proof**

this is the [representation invariant](ir.md#the-representation-invariant) doing
its job rather than a flaw: exactness is narrowing, narrowing needs a proof or a
check, and the gradual edge has no proof. the check goes at the existing
`arguments` / `assignments` [soundness positions](../../features/soundness.md)

the pleasant part is that the check `final` needs is *cheaper* than the one it
replaces. a subtype check walks the MRO; an exactness check is one pointer
compare:

```c
if (Py_TYPE(arg) != &Widget_Type) { CPy_TypeError("Widget", arg); goto error; }
```

so on the gradual boundary `final T` is faster than `T`, and past the boundary it
is faster still. that is an unusual shape for a safety check and worth stating
plainly

### the trap: exactness has two spellings

`RestrictedType::from_type_expression` reduces `final C` to plain `C` when `C` is
already a `@final` class, on the grounds that the restriction adds nothing. it is
right about the type, and it means **the mapper never sees `Type::Restricted` for
the most obviously exact case**. reading only `Type::Restricted(Final, _)` would
devirtualize open classes annotated `final` and miss every `@final` class

the mapper must therefore treat two inputs as one bit:

```text
exact(ty) = matches!(ty, Type::Restricted(r) if r.modifier() == Final)
         || matches!(ty, Type::NominalInstance(n) if n.class().is_final())
```

## `literal T`: compile-time values

`literal T` accepts a value whose type is a *literal type*. the compiler's
interest splits cleanly in two, and conflating the halves would overclaim

### the half that is a constant

when the value's type is a `Type::LiteralValue` — `Literal[8]`, `Literal["x"]`,
`b"y"`, an enum member — the value is known at compile time. it folds, it never
allocates, and it is emitted as a module static, immortal for the module's
lifetime, so it carries no refcount traffic at all

the prize here is **const-generic specialization**. a `literal` parameter is a
natural monomorphization key, and unlike a type key it is a *value* key:

```by
def pad(s: str, width: literal int) -> str: ...

pad(name, 8)
```

specializing `pad$8` makes `width` a C constant, which unrolls the loop and folds
every comparison against it. this is the const-generics-lite that
[range analysis](optimizations.md#ranges-from-the-type-system) was reaching for,
arriving from a different direction, and it is squarely outside what mypyc can
express

it also inherits monomorphization's hazard, so it inherits its policy: value keys
count against the same `compile.monomorphize-limit`, and the erased body is
always emitted ([optimizations](optimizations.md#monomorphization))

### the half that is only a provenance guarantee

`literal str` is not in this category. it reduces to `LiteralString`, which means
"built only from literals" — and `a + b` over two `LiteralString`s is a
`LiteralString` built at runtime. so a `literal str` parameter is **not** a
compile-time constant, is not necessarily interned, and buys the compiler
nothing. the guarantee is about where the characters came from, not about when
they were known

this matters because the two spellings look identical in source and land on
different `Type` variants. the mapper keys on `Type::LiteralValue`, not on the
`literal` keyword

### a small third thing

`literal list[*]` is `list[Never]`, whose only inhabitant is `[]`. a place typed
that way is a statically empty container: its `len` is `0`, and a `for` over it
is dead code the reachability pass deletes

## `frozen` / `erased T`: an erasure with no representation content

the review's verdict is that `frozen` should not ship as specified, and should
be redefined as declaration-site sugar for `SafeVariance[T]` on every input
occurrence of `T` (and renamed, since `frozen data class` already means something
else). the compiler independently wants the same outcome, for a reason the review
does not raise

### as proposed, it is a pessimization with no recovery

as specified, both the call site *and* the body see `T`'s upper bound. an input
typed at `object` is a boxed input, at every call, forever — and since the caller
has the precise type and the callee refuses it, every call pays a box the program
did not need. there is no optimization that recovers it, because the information
was discarded at the declaration

### redefined as `SafeVariance[T]`, it costs nothing

`SafeVariance[T]` keeps the **call site precise** and widens only inside the
body. that difference is everything: the erasure is a *typing* restriction whose
whole purpose is to stop the body storing the value back into `T`-typed covariant
storage, and ty enforces that statically before the compiler runs

so the erasure has **no representation content**, and the mapper rule is:

> map `SafeVariance[T]` to the **call-site** type, not to the bound

a parameter written `SafeVariance[int]` keeps an unboxed `int` register. the body
may not assign it into a `T` slot, which is a fact about what code ty accepted,
not about what the bits look like. the naive mapping — follow the declared type,
land on `object` — would silently box every erased parameter in the program, and
it is the kind of pessimization that is very hard to notice after the fact

## `single T`: declared discriminated unions for generics

`single T` means the instance's type argument is **witnessed by exactly one
contained value**, the slot is read-only after construction, and the constructor
parameter binding it is required. narrowing the witness narrows the
specialization, so `A[X | Y]` decomposes to `A[X] | A[Y]`

this is the largest codegen win of the five, and it is worth more to the compiler
than to the checker

### the witness is the tag

[sealed hierarchies](optimizations.md#sealed-hierarchies-become-tagged-unions)
already become tagged unions. `single` extends the same representation to
*generic* classes, which nothing else in python gets:

```by
class Box[single T]:
    def __init__(self, t: T): ...
    def get(self) -> T: ...
```

```c
/* Box[int | str], not escaping */
typedef struct { uint8_t tag; union { int64_t i; PyObject *s; } w; } Box_int_str;
```

and where it does escape and keeps a real object header, it still needs **no
discriminant field**: the single witness already distinguishes the arms, so the
tag is derived rather than stored

```c
#define Box_is_int(b)  CPyTagged_CheckShort(((BoxObject *)(b))->witness)
```

the read-only requirement is what makes this sound — a writable slot would let
the witness move after construction, and the tag would go stale. it also makes
the instance immutable in its `T` slot, which feeds
[free-threading](optimizations.md#free-threading) directly

### it deletes the `__orig_class__` probe, and then the stamp

this is the sharpest single win available anywhere in this design

[parametric type tests](../../features/parametric-type-tests.md) fall back, for
an undecidable user-generic target, to reading the value's `__orig_class__` — an
attribute lookup, a tuple index, and a type comparison. under `single`, the same
question is answered by the witness:

```c
/* x is Box[int], today's fallback */
PyObject *oc = PyObject_GetAttr(x, s_orig_class);   /* dict lookup, may raise */
/* ... index the args tuple, compare the type ... */

/* x is Box[int], under single */
CPyTagged_CheckShort(((BoxObject *)x)->witness)     /* one load, one test */
```

and the win compounds backwards into construction. `__orig_class__` is stamped by
routing the call through `A[int](…)`, which [type reification](../../features/type-reification.md)
emits for every inferred specialization. if no test needs the stamp, the
construction does not need to pay for it

the gate on skipping the stamp is escape, not just `single`: `__orig_class__` is
an ordinary observable attribute, so it may only be dropped for a class that does
not cross into interpreted code — a tier-3, not-in-`api.lock` class. that is the
same boundary [the lockfile](optimizations.md#the-lockfile-as-a-closed-world-boundary)
already draws

### narrowing devirtualizes the whole arm

```by
def f(b: Box[int | str]) -> int:
    match b.get():
        case int() as n: return n
        case str() as s: return len(s)
```

the `match` is a tag test on the witness. inside the first arm the instance is
`Box[int]`, so `b.get()` returns an unboxed `int` and every other method call on
`b` in that arm is monomorphic. one branch buys specialization for everything
downstream of it — the same shape `sealed` gives, now reachable from a generic

### three traps, and none of them is the syntax

**the member cutoff must be shared.** the review's finding 7 requires a
member-count cutoff above which decomposition is declined. the compiler needs one
too, for the same combinatorial reason — `Box[Literal["a", …, "z"]]` is 26
specializations — and it must be the **same constant**. if the checker decomposes
where the compiler declines, a program type-checks against a shape the compiler
cannot represent, and the fallback has to be discovered at codegen time instead of
declared

**both spellings must converge in the mapper.** finding 8 notes that `single`
breaks the member-wise `X <: A | B` fast path. the codegen consequence is
sharper: `Box[int | str]` and `Box[int] | Box[str]` are the same type and must be
the same *representation*, or an assignment between two equivalent static types
needs a conversion nobody wrote. decomposition therefore has to happen **in the
mapper, before representation selection**, not as a late peephole. this is an
easy bug to ship and a miserable one to find

**fluid specializations can move the witness type.** see below

### one request back to the language design

the compiler needs to know **which member holds the witness**, not merely that
one exists. a boolean "is single" on the type parameter is enough for the
checker's decomposition rule and useless for codegen — without the member
identity there is no field to test and no tag to derive

so: record the witness member on the type parameter. the review's open question
"is `single` declared or inferred?" is orthogonal — inferring the property means
inferring the member, and the member is what has to be stored either way

## `overlapping T`: a guarantee, not an optimization

under the review's recommended reading, `overlapping` is a pure diagnostic over
an unchanged solver: it never changes an inferred type. so its direct codegen
impact is **nil**, and saying so is the honest answer

the indirect effect is real, though, and it is worth naming precisely. a
union-valued typevar solution is exactly what defeats
[monomorphization](optimizations.md#monomorphization): `T = int | str` yields one
boxed body where two unboxed ones were available. `overlapping` rejects those
solutions at the call site, so an `overlapping` generic is **statically
guaranteed monomorphizable**

that makes it a performance-*predictability* annotation. it does not make code
faster; it stops code from silently becoming slower, which for a compiled
language is a different and sometimes more valuable thing

it also gives the review's finding 15 a second consumer. the ecosystem-wide lint
it recommends — `disjoint-typevar-solution`, applied to every generic call — is
the same analysis `by compile --annotate` needs to answer "why was this function
not specialized". one implementation, two framings

## the fluid-specialization hazard

this one is not a feature, it is an interaction, and it is the most likely of
anything here to produce a wrong answer rather than a slow one

a register's `RType` is fixed for its whole live range. a
[fluid specialization](../../features/fluid-specializations.md) is not:

```by
a = A(1)        # A[int]
a.y("s")        # widens to A[int | str]
```

if the irbuilder types `a`'s register from the specialization at the *definition*
site, the widening later in the flow changes the type of a live register, which
the verifier will reject — if it is lucky. the rule is that the builder must use
the **locked** specialization, the one the binding settles at, for the register's
representation from the start

`single` makes this consequential rather than academic. a fluid widening from
`Box[int]` to `Box[int | str]` moves the value from an unboxed-witness
representation to a tagged one — the bits change shape, not merely the label. the
builder must either take the locked type up front, or emit an explicit conversion
at the widening point and treat the two as distinct registers. taking the locked
type is much simpler and is what the design assumes

## mapper rules

the additions to `Type → RType` ([ir](ir.md#mapping-ty-types-to-rtypes)):

| ty type                                    | `RType`                          | note                                       |
| ------------------------------------------ | -------------------------------- | ------------------------------------------ |
| `Restricted(Final, C)`                     | `RInstance { exact: true }`      | direct dispatch, pinned layout             |
| `NominalInstance(C)` where `C` is `@final` | `RInstance { exact: true }`      | the same bit by the other spelling         |
| `Restricted(Literal, T)`                   | as `T`                           | the keyword alone carries nothing          |
| `LiteralValue(v)`                          | a constant, statically allocated | the const-specialization key               |
| `LiteralString`                            | `str`                            | provenance only, no representation content |
| `SafeVariance[T]` in a parameter           | the **call-site** type           | never the bound                            |
| `A[X \| Y]` where `A` is `single`          | tagged `RUnion`, tag = witness   | decomposed before representation selection |
| `A[X] \| A[Y]` where `A` is `single`       | the same tagged `RUnion`         | the two spellings must converge            |

`RInstance` gains an `exact` bit. `RUnion` needs no new variant — the `sealed`
representation carries `single` unchanged, which is the pleasant part

## what this changes in the rest of the suite

- [ir](ir.md) — `RInstance.exact`; the two mapper rows above; the mapper is where
    `single` decomposition happens
- [optimizations](optimizations.md) — `final` extends devirtualization to open
    classes; `final` + `local` is the complete `StackAlloc` precondition;
    `literal` adds a value-keyed monomorphization axis; `single` extends the
    tagged-union representation to generics
- [plan](plan.md) — the `True`/`1` delta acquires an opt-out; the shared member
    cutoff is a new cross-component invariant to test
- nothing in [technology](technology.md) or [runtime](runtime.md) moves. all five
    modifiers are erase-only, so the transpiled python is unchanged and the
    [differential harness](plan.md#differential-testing) keeps working as-is
