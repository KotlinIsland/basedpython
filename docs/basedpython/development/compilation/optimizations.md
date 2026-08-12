# type-directed optimizations

every optimization here is unavailable to a compiler reading PEP 484 types. each
one exists because basedpython lets a program *state* something that mypy can
only try to infer, and usually cannot

## ranked

| #   | optimization                                                     | source feature                  | payoff  | cost   |
| --- | ---------------------------------------------------------------- | ------------------------------- | ------- | ------ |
| 1   | [precise types everywhere](#sound-types-the-enabler)             | `analysis.sound-types`          | huge    | none   |
| 2   | [error-path elision](#error-path-elision)                        | `raises`                        | huge    | low    |
| 3   | [closed-world binding](#the-lockfile-as-a-closed-world-boundary) | `api.lock`, `final`             | huge    | medium |
| 4   | [escape analysis](#escape-analysis-that-crosses-calls)           | `local`, `once`                 | huge    | medium |
| 5   | [tagged unions](#sealed-hierarchies-become-tagged-unions)        | `sealed`, `enum class`          | large   | medium |
| 6   | [monomorphization](#monomorphization)                            | reified generics                | large   | high   |
| 7   | [range analysis](#ranges-from-the-type-system)                   | literal types, `Array[Dim + 1]` | large   | medium |
| 8   | [exact floats](#exact-floats)                                    | no number promotions            | medium  | none   |
| 9   | [inline blocks](#trailing-lambdas-are-inline-functions)          | trailing lambdas, `once`        | medium  | medium |
| 10  | [null-check elision](#narrowing-is-a-proof)                      | narrowing, `not T`, `&`         | medium  | low    |
| 11  | [intrinsics](#intrinsics)                                        | extensions, `Character`, regex  | medium  | low    |
| 12  | [fixed layout](#fixed-layouts-and-always-defined-attributes)     | `data class`, `init` mods       | medium  | low    |
| 13  | [free threading](#free-threading)                                | `frozen`, `local`               | large\* | high   |

\* payoff is unbounded but only for code that is actually parallel

three more are planned rather than available, and two of them are large enough to
change the design rather than extend it. they are worked out separately in
[planned features](planned-features.md):

| optimization                                 | source feature | payoff | cost   |
| -------------------------------------------- | -------------- | ------ | ------ |
| tagged unions for *generics*                 | `single T`     | large  | high   |
| use-site devirtualization and layout pinning | `final T`      | large  | low    |
| value-keyed specialization                   | `literal T`    | medium | medium |

## sound types, the enabler

python's [gradual guarantee](../../features/sound-types.md) forces a checker to
infer `Any` for anything unannotated. for a compiler, `Any` means: box it, look
it up at runtime, check it on the way back in. mypyc spends most of its
performance ceiling on this, and the only fix available to a mypy user is to
annotate every local by hand

`analysis.sound-types` deletes the problem:

```by
def f(a=1):        # `a` is `int`, not `Any`
    total = 0      # `total` is `int`, not `Any`
    for x in a:    # element type flows through
        total += x
    return total
```

under gradual rules this function is entirely `object`-typed: boxed accumulator,
generic `+`, boxed loop. under sound types every register is `int` and the loop
body is native integer arithmetic

this costs nothing to implement in the compiler — it is a property of the types
it is handed. it is listed first because it multiplies the value of everything
below it, and because it means basedpython code gets compiled well *without an
annotation campaign*

## error-path elision

mypyc emits an error check after **every** native call. that is not a heuristic
gap — its `Call` op derives `error_kind` from the callee's *return type* alone
(`ERR_MAGIC`, or `ERR_MAGIC_OVERLAPPING` when the sentinel overlaps a valid
value), and `FuncDecl` carries no "cannot fail" bit for it to consult. mypyc
tracks failure finely per *operation* and not at all per *function*:

```c
CPyTagged r1 = f(x);
if (r1 == CPY_INT_TAG) goto error;      // after every single call
CPyTagged r2 = g(r1);
if (r2 == CPY_INT_TAG) goto error;
```

[exception tracking](../../features/exceptions.md) makes that check *typed*. a
`raises Never` function cannot fail, so it gets the
[`native-infallible`](ir.md#calling-conventions) convention and its callers emit
nothing:

```by
def f(x: int) -> int raises Never: ...
def g(x: int) -> int raises Never: ...

def h(x: int) -> int raises Never:
    return g(f(x))
```

```c
int64_t h(int64_t x) { return g(f(x)); }   // no error path exists
```

three effects compound:

- **fewer branches.** in tight numeric code the error checks can be a third of
    the emitted branches
- **no error-value reservation.** an infallible `i64` return does not need a
    sentinel, so the whole `error_overlap` complication — where a valid value
    doubles as the error signal and forces a `PyErr_Occurred()` call to
    disambiguate — disappears for that function
- **straight-line blocks.** an error edge is a CFG edge; removing it lets the C
    compiler keep basic blocks large, which is what its own optimizer wants

partial sets pay too. a function declared `raises ValueError` and called inside
`try: … except ValueError:` needs no propagation past the handler, and the
handler's dispatch is a known single type instead of an exception-matching
sequence. a function whose set is `A | B` and whose caller catches `A | B`
generates a two-way switch on the tag, not a chain of `PyErr_GivenExceptionMatches`

`raises ...` opts out and gets today's behaviour. that is the correct default
for anything crossing into interpreted code

## the lockfile as a closed-world boundary

the hard question for any python compiler is "can this function be replaced at
runtime". mypyc answers it with a compilation-unit heuristic and a pile of
documented caveats

basedpython already has an artifact that answers it exactly. the
[api lockfile](../../features/api-lock.md) is a committed, reviewed, line-oriented
statement of the public type-level surface. at tier 3 it stops being only a
review artifact and becomes the **ABI contract**:

- **in the lockfile** → the symbol keeps a stable python-visible surface: a
    `python` entry point, its name, its argument names, its docstring, its
    monkey-patchability
- **not in the lockfile** → the symbol is internal to the unit. it may be
    inlined, renamed, given an unboxed signature, monomorphized into several
    copies, or deleted entirely if nothing calls it

this is a better boundary than any compiler-internal heuristic for a reason that
has nothing to do with compilation: **it is reviewable**. the diff that changes
what the optimizer may assume is the same diff a human already approves. a
regression in the ABI is visible in code review before it is visible in a
crash report

`by compile --tier=3` therefore requires a current lockfile and refuses if
`by generate-api-file` would produce a different one — the same check CI already
runs

`final` gives the same thing at class granularity, in any tier: a `final class`
cannot be subclassed, so every method call on it is a direct call, and a
`sealed class` gives the closed subclass set, so a call on the *base* is a
switch over a known-small set rather than a vtable indirection

and the planned use-site [`final T`](planned-features.md#final-t-exactness-at-the-use-site)
gives it at *place* granularity, which is finer than either: a hot function can
demand an exact `Widget` at one parameter while the class stays open for
everyone else

## escape analysis that crosses calls

[local lifetimes](../../features/local-lifetimes.md) already implement a static
escape analysis, checked by ty today. its purpose is expressing intent, but a
compiler reads it as something else entirely: a **declared, interprocedural,
type-checked non-escape proof**

no python compiler has this. escape analysis is normally intraprocedural and
dies at the first call into unknown code — the compiler must assume a callee
keeps everything it is given. `local` says it does not, and that promise is
checked, not trusted

three optimizations follow

### borrowed arguments

a `local` parameter is passed **without a reference count**. no `IncRef` on
entry, no `DecRef` on exit, on either side of the call:

```by
def scan(local buf: bytes) -> int:
    ...
```

the refcount pass ([ir](ir.md#the-pass-pipeline), pass 18) already elides pairs
*within* a function; `local` extends that across the boundary. for a hot loop
calling a helper per iteration, this removes two atomic operations per
iteration under free-threading, and two dependent memory writes under the GIL

### stack allocation

if every use of a freshly-constructed object is `local`, and it is not stored,
returned, or captured, it never needs a heap allocation:

```by
def area(local r: Rect) -> float: ...

def total(rects: list[(float, float)]) -> float:
    sum = 0.0
    for w, h in rects:
        sum += area(Rect(w, h))     # Rect never escapes
    return sum
```

`Rect(w, h)` becomes a `StackAlloc` — a frame slot, no `PyObject` header, no
allocator call, no refcounting. with a `final` or `data class` `Rect` the C
compiler then applies scalar replacement and the struct disappears too, leaving
two `double`s in registers

non-escape is only half the precondition, though. a frame slot needs a *size*,
and an open class gives only "unknown" — a value typed `Rect` may be a subclass
with more fields. so the complete rule is **`local` proves it does not escape and
exactness proves how big it is**, where exactness comes from a `@final` class
today and, once it lands, from a use-site
[`final T`](planned-features.md#final-t-exactness-at-the-use-site) on any class
at all

### linear callbacks

a `once` callback is called exactly **once**, and that is checked. so it can be
inlined unconditionally with no code-size analysis, and its closure — normally a
heap-allocated environment object — becomes frame slots

## sealed hierarchies become tagged unions

an [enum class](../../features/enums.md) is an algebraic sum type whose variants
subclass the enum, and a [sealed class](../../features/sealed-classes.md)
publishes the complete subclass set. compiled, that is a discriminated union:

```by
enum class Expr:
    case Lit(value: int)
    case Add(left: Expr, right: Expr)
    case Neg(operand: Expr)

    def eval(self) -> int raises Never:
        match self:
            case Expr.Lit(v): return v
            case Expr.Add(l, r): return l.eval() + r.eval()
            case Expr.Neg(o): return -o.eval()
```

- the value is `{ uint8_t tag; union { … } }`, not three heap objects
- `match` is a `SealedSwitch` — a C `switch` the compiler turns into a jump
    table — not a chain of `isinstance`
- exhaustiveness is already proven by ty, so there is no default arm and no
    "unreachable" fallback to keep the C compiler happy
- combined with `raises Never`, `eval` compiles to a recursive function over a
    tagged struct with no allocation, no refcounting, and no error path — which
    is to say, to what you would have written in C

nested payloads that are themselves recursive still need indirection, so `Add`
holds pointers. the flattening applies to the *tag dispatch* and to leaf
variants unconditionally, and to whole values where they do not escape

this is the optimization with the largest gap between compiled and interpreted
basedpython, because the interpreted lowering — a dataclass hierarchy — is the
most expensive possible encoding of the same idea

the planned [`single T`](planned-features.md#single-t-declared-discriminated-unions-for-generics)
extends this same representation to *generic* classes, where the tag is the
instance's one witness value rather than a stored discriminant. it reuses
`RUnion` unchanged, which is why it is the largest of the planned wins and why it
is worth designing the sealed representation to be reusable now

## monomorphization

standard python erases type parameters, so mypyc compiles one boxed body per
generic function. [reified generics](../../features/reified-generics.md) make the
type argument a real runtime value, and — more usefully for a compiler — make
the instantiation **syntactically explicit at the call site**:

```by
def sum_all[T in (int, float)](xs: list[T]) -> T:
    ...

sum_all[int](a)
sum_all[float](b)
```

the monomorphize pass emits `sum_all$int` with an `int` element representation
and `sum_all$float` with an unboxed `f64` one. inside each, the element type is
concrete, so the loop body is native arithmetic instead of `PyNumber_Add`

policy, because monomorphization is how compilers get slow:

- specialize per **distinct `RType` tuple**, not per static type — `list[str]`
    and `list[bytes]` share a body
- only for instantiations reachable *within the unit*, which tier 3 knows
    exactly
- capped at `compile.monomorphize-limit` (default 8) per function; beyond that,
    fall back
- the erased body is **always** emitted, so cross-unit and interpreted callers
    have something to call, and so the cap is never a correctness cliff

[type mappings](../../features/type-mappings.md) are the ideal case: `T in (int, float)`
bounds the instantiation set to two, statically, with no call-site analysis needed at
all

two planned modifiers act on this pass from opposite ends.
[`literal T`](planned-features.md#literal-t-compile-time-values) adds a **value**
key alongside the type key, so `pad(s, 8)` can specialize on the constant `8` and
unroll. [`overlapping T`](planned-features.md#overlapping-t-a-guarantee-not-an-optimization)
adds no optimization at all, but rejects the union-valued typevar solutions that
*defeat* this pass — so an `overlapping` generic is statically guaranteed to
specialize, which is a promise about predictability rather than about speed

## ranges from the type system

mypyc's [future work](https://github.com/python/mypy/blob/master/mypyc/doc/future.md)
asks for integer range analysis, to pick untagged representations, skip overflow
checks, and remove index checks. it is an inference problem there. basedpython
already carries part of the answer in the annotations, through
[literal types](../../features/literal-types.md),
[symbolic type operations](../../features/symbolic-type-ops.md), and integer
type parameters:

```by
def clamp(x: Literal[0] | Literal[1] | Literal[2]) -> int: ...

def extend[Dim: int](a: Array[Dim]) -> Array[Dim + 1]: ...
```

- a union of integer literals is a known finite set, so it picks a `u8` with no
    tag, no overflow check, and no boxing
- `2 ** 8` folds to `Literal[256]` in a type position, so a size written as an
    expression is still a constant to the range pass
- `Array[Dim + 1]` keeps shape arithmetic symbolic until specialization, so a
    relationship written once at the signature is available at every use — the
    machinery exists today for type checking, and the range pass is a second
    consumer of it

a conventional interval analysis (pass 05) covers everything unannotated — loop
induction variables, `len()` results, `range()` bounds. the declared facts seed
it, which is what makes it effective across call boundaries, where interval
analysis normally gives up

### the notation that is missing

the payoff would be much larger with a **bounded integer type** — a way to say
"an `int` in `0..<n`" directly, so that an index parameter can be *declared*
in-bounds against a shape parameter rather than inferred:

```by
def get[N: int](a: Array[N], i: int < N) -> Element: ...   # proposed, does not parse today
```

that turns a bounds check into a static proof, which is the single most valuable
thing range analysis can do for numeric loops — a per-element bounds check is
what blocks vectorization. the type system already has the pieces
([bound ranges](../../features/bound-ranges.md) for two-ended bounds, symbolic
operations for the arithmetic); what is missing is the surface syntax and the
comparison-as-bound semantics. it is worth designing as a language feature on
its own merits, and the compiler is the reason to prioritize it

## exact floats

python's typing spec special-cases `float` to mean `int | float`. so mypy sees
`x: float` and a compiler must accept a `PyLongObject` there, which means a
branch and a conversion at every use, and a boxed representation for anything
stored

[basedpython does not do this](../../features/no-number-promotions.md). in a
`.by` file `float` is exactly `float`:

| `x: float` under        | representation                               |
| ----------------------- | -------------------------------------------- |
| the typing spec (mypyc) | `PyObject *`, or `double` + an int fast path |
| basedpython             | `double`                                     |

`list[float]` becomes a native `double` buffer, an arithmetic expression over
`float`s becomes straight SSE, and none of it needs a guard. the same applies to
`complex`. this is a free optimization: it is a consequence of a decision the
language already made for clarity, and the compiler just gets to keep it

## trailing lambdas are inline functions

a [trailing lambda](../../features/trailing-lambdas.md) block binds exactly one
argument, and its parameter can be declared `local` or `once` in the callee's
signature — which constrains what the block may do with the receiver:

```by
extension list:
    def each(self, local body: (local Element) -> None) -> None:
        for x in self:
            body(x)

xs.each():
    print(it)
```

`body` is `local` (does not escape) and called in a loop. the inline pass
substitutes the block into `each`'s body and then `each` into the call site,
giving a plain loop over `xs` with `print(it)` in it — no closure allocation,
no per-element call, no boxing of `it`

this is exactly kotlin's `inline fun` optimization, and basedpython arrives at it
by the same route: a syntactic form for blocks, plus lifetime annotations on the
parameter that receives them. the difference is that kotlin needs an `inline`
keyword and we can derive it from `local` / `once`

## narrowing is a proof

ty's narrowing already produces [intersections](../../features/intersection.md)
and [negations](../../features/not-type.md). a compiler reads them as removed
checks:

| after narrowing       | what the compiler drops                        |
| --------------------- | ---------------------------------------------- |
| `A & not None`        | the null test on every subsequent use          |
| `x is Shape.Circle`   | the tag test inside the branch                 |
| `A & B` on a protocol | the structural lookup — both members are known |
| a `sealed` match arm  | the discriminant is a constant in the arm      |

[parametric type tests](../../features/parametric-type-tests.md) go further: an
`is list[int]` check that ty accepts as reifying `T` gives the branch a concrete
element representation, so the container read inside it is unboxed

## intrinsics

several basedpython surfaces are defined today as *lowerings to python
expressions*. compiled, they lower to native calls instead:

| surface               | interpreted lowering                | compiled                                        |
| --------------------- | ----------------------------------- | ----------------------------------------------- |
| `s.character_count`   | `len(_by_graphemes(s))`             | a rust segmenter over the UTF-8 buffer, no list |
| `s.first` / `s.last`  | list index into a materialized list | a single cluster scan from either end           |
| an `extension` method | a module-level function call        | a direct native call, inlinable                 |
| `?.` chains           | nested conditionals                 | a single null test over the whole chain         |
| `??`                  | a conditional expression            | a `cmov`                                        |

## a str that is known to be a str

`str` is the one builtin the abstract object protocol costs the most on, because
almost every operation on it is cheap once the type is settled and the protocol's
whole job is settling the type. so a `str`-typed operand buys three things, none
of which needs a source annotation beyond the type already being `str`:

| operation | through the protocol                                   | knowing it is a `str`              |
| --------- | ------------------------------------------------------ | ---------------------------------- |
| `len(s)`  | `PyObject_Length`, a slot dispatch                     | a field read                       |
| `s[i]`    | `mp_subscript`, with the index boxed to get in         | a character read at the known kind |
| `a == b`  | `PyObject_RichCompareBool`, a reflected-operand search | length, kind, `memcmp`             |

each is guarded on the **exact** type, because a subclass may have overridden the
operation, and a fast path that ignored that would be a different answer rather
than a faster one

### growing rather than copying

`a + b` on two strings has a worse problem than dispatch. `PyUnicode_Concat`
always allocates a new string and copies both operands into it — it has to,
because it cannot know whether anyone else can see `a`. so a string built a piece
at a time is copied once per piece, and building it is quadratic in its own
length.

cpython has the same problem and half a fix: `BINARY_OP_INPLACE_ADD_UNICODE`
grows the left operand in place, but only when the operand *is* the local the
result is stored back into. `out = out + a + b` does not match that shape — the
first `+` is followed by another `+` rather than by the store — so the whole of a
chained concatenation deoptimizes back to copying.

what makes the in-place form legal is that nothing else can see the string, and
what a register machine can prove is exactly that: a concatenation whose left
operand register is **not read again** can take the reference with it, leaving the
count at one. `by_opt::str_append` marks those, and codegen empties the register
into the call.

the same liveness settles the one behavioural question it raises. a failed append
cannot put the reference back, so a register a handler could still read must keep
its own — and a register a handler could read is live across the error edge, which
is an edge the analysis already follows. the condition that makes the append fast
is the condition that makes its failure unobservable

### regex shapes

[regex group types](../../features/regex-groups.md) mean the pattern is parsed
at check time and its group structure is statically known. when the pattern is a
literal, we can go further and **compile the pattern itself** into the extension:
`m.group(1)` becomes a struct field read on a match object with a fixed layout,
and the matcher is a native DFA rather than a call into `_sre`

this is a large piece of work and it is scoped out of the initial milestones,
but the type system has already done the hard half — it knows the shape

## fixed layouts and always-defined attributes

mypyc keeps a bitfield per class recording which attributes are currently
defined, and checks it on every read, because python lets `__init__` skip an
assignment. it lists "always defined attributes" as future work

basedpython gets it from declarations:

- a [`data class`](../../features/modifiers.md) already emits `slots=True` and a
    generated `__init__` that assigns every field — every attribute is always
    defined, so the bitfield and its check disappear
- [`init` method modifiers](../../features/init-method.md) declare and assign in
    one place, with the same consequence
- a `private` field is name-mangled and cannot be assigned from outside, so its
    definedness is a whole-class property rather than a per-instance one

what is left is the bitfield only for classes that genuinely assign
conditionally, which is the case it was designed for

### a frozen field is read once

**done.** a `frozen data class` cannot have its fields changed after the
constructor wrote them, so two reads of one field are a *single* read — and the
part a type system is needed for is that this holds **across an arbitrary call**.
an optimizer that must assume any call may mutate any object has to reload.

two things are worth recording, because both were found rather than designed:

- `frozen` in the ir meant "emit no setters", which a generator's state class and
    a closure environment both set while their fields change on every step. that is
    a different question from immutability, and conflating them gave a wrong answer
    within seconds of the differential harness seeing it. `ClassIr::immutable` is
    now the question the fold asks, and the setter rule is derived from it
- the fold still invalidates on a `SetField` to the same field. the licence comes
    from the declaration, but the fold rests on what the ops do

### an exact place keeps its direct call

**done.** a class that is decorated, extends another, or is extended by another is
emitted as a mutable heap type and gives up the direct method call — python can
rebind a method on one, or override it in a subclass. that is a fact about the
*class*; `@final` is a fact about the **place**, and it re-licenses the direct call
because no subclass can exist.

⚠️ `sealed` is **not** exactness, and the difference is easy to miss: it closes the
world *outside* the declaring module and says nothing about a subclass inside it.
treating it as exactness produced a direct call to a base's method where a subclass
declared a few lines down overrode it. sealing licenses a **switch** over the known
subclasses (§5), which is a different and weaker thing than a direct call.

## free threading

this is the speculative section, and the one with the highest ceiling

cpython 3.14 supports free-threaded builds. the obstacle to using them is not
the GIL, it is that nothing in the language says which values may be shared. so
runtimes fall back to atomic refcounting everywhere and to defensive locking

the floor is not an optimization at all but a requirement: an extension must
declare `Py_mod_gil = Py_MOD_GIL_NOT_USED` or importing it re-enables the GIL
process-wide ([technology](technology.md#free-threading-is-a-design-constraint-now-not-a-migration-later)).
everything below is what can be built on top of that floor

basedpython can say it:

- **`frozen data class`** is deeply immutable — shareable across threads with no
    synchronization at all, and (with immortalization) with no refcount traffic
- **`local`** proves a value does not escape the call, so it cannot be reached by
    another thread, so its refcounting can stay non-atomic on the owning thread
- **`once`** proves a callback runs exactly once, which is the linearity property
    a work-stealing scheduler wants
- **`raises`** makes the failure modes of a parallel region a declared, finite
    set instead of an open one

the concrete deliverable is a `parallel` form over a `local`-parameterized block
whose safety is checked rather than documented:

```by
results = xs.parallel_map(): compute(it)
```

accepted only when the block captures nothing mutable, its parameter is `local`,
and its return type is `frozen` or a primitive — all of which ty can already
decide. this is the thing basedpython could have that no other python dialect
can, and it is worth designing toward even though it lands last

## what we deliberately do not optimize

- **dict and set iteration order**, which is observable and must be preserved
- **`is` on small ints and interned strings**, which is observable. note that
    unboxed fixed-length tuples already break `is` in mypyc; we take the same
    trade and document it in [plan](plan.md#semantic-deltas)
- **`__dict__` on native classes**, which we do not synthesize at all unless a
    class opts in
- anything where the win is smaller than the divergence risk. the differential
    harness is the arbiter, and a passing benchmark that fails it is not an
    optimization
