# `type def` — user-defined type functions

## summary

a `type def` is an ordinary function whose arguments are types, whose result is a
type, and whose call syntax is subscription in a type expression:

```by
type def F[X]:
    if X <= int:
        return int
    return str

def f() -> F[bool]     # int
def g() -> F[float]    # str
```

the body is real basedpython, executed by a real python interpreter. it may do
anything a python function may do — read files, hit the network, spawn processes.
the language does not restrict it; the author owns the footgun

the whole design turns on one sentence: **a type function is not "run during type
checking", it is a memoized oracle whose observations are recorded**. everything
below — the execution engine, the cache, the incrementality story, the error
messages — falls out of that

## mental model

`type def F[X]` desugars to

```by
def F(X: TypeInfo, /) -> TypeForm | TypeError: ...
```

with two differences: it is called with `[]` from a type expression, and it is
called by the type checker rather than by the program

everything else follows the ordinary rules. the body is type-checked like any
other function body. `X` is a `TypeInfo` inside it. a declared return type is
checked normally, so a missing `return` is caught by the existing
`invalid-return-type` diagnostic rather than by a bespoke check. type-parameter
bounds become preconditions:

```by
type def Widen[X: int]:      # `Widen[str]` is an ordinary invalid-argument error,
    return X | None          # reported before anything is executed
```

variadics and keyword type arguments map onto the corresponding parameter kinds,
reusing [keyword subscripts](../features/kw-subscript.md):

```by
type def Shape[T, *Dims, order: str = "c"]: ...
# Shape[float, 2, 3, order="f"]  →  T=float, Dims=(2, 3), order=Literal["f"]
```

## bounds, and `X: int`

`type def F[X: int]` is a **bound**, not a value parameter. it keeps exactly the
meaning it already has everywhere else in the language — `def extend[Dim: int](a: Array[Dim]) -> Array[Dim + 1]` — so `X` accepts `int`, `bool` and `Literal[3]`
alike, and the body receives a `TypeInfo`, statically `TypeInfo[int]`

this matters because the desugaring invites the opposite reading. once
`type def F[X]` is `def F(X: TypeInfo, /)`, the annotation in `[X: int]` looks
like an ordinary parameter annotation, i.e. "the body gets an `int`". choosing
that reading would fork the meaning of `[X: int]` between `type def` and every
other binder in the language, and would make `F[int]` — a perfectly good type
argument that is not a single value — unrepresentable. the bound reading wins

bounds earn their keep in four ways:

- **they are checked before anything executes.** a violation is an ordinary
    invalid-type-argument error at the application site. no worker is spawned, no
    cache entry is created. a bound is always cheaper than the equivalent
    `return TypeError(...)`, and should be the first thing an author reaches for
- **they are the only part of a type function that says anything about symbolic
    arguments.** `F[T]` cannot run, but `T`'s own bound can be compared against
    `X`'s, so `def f[T: str](x: F[T])` is rejected at the *definition*, not once
    someone specializes it. only the decidable part of the bound can be checked
    that early — see [deferred arguments](#bounds-on-a-deferred-argument)
- **they type the body.** under `X: int`, `X.literal` is `int | None` rather
    than `object | None`, and the `None` is not a formality — it is what `F[int]`
    produces. the author is forced by ordinary type checking to decide what a
    non-literal argument means, which is precisely the case they would otherwise
    forget
- **they narrow the tree.** the precondition prunes whole regions of the
    argument space before the decision tree is consulted

### there are no value parameters

a type argument is always a type. `Pad[4]` passes `Literal[4]`, not the number
four — no value ever appears in a type position, and no parameter kind could
make one appear there. the only question in the neighbourhood is what python
object the *executing body* binds, which is a property of the process boundary,
not of the type-parameter list

so a function that is really arithmetic reads through the reflection api:

```by
type def Pad[X: int]:
    n = X.literal
    if n is None:
        return TypeError("Pad needs a literal size")
    return Array[n + 1]
```

the `None` there is the annoyance worth removing, and it is removable **at the
type level**, where it belongs: it exists because `X: int` admits `int` itself,
not because of anything about binding. the bound is under-specified, so tighten
the bound. that wants a type form for "some literal of this type", generalising
`LiteralString` ([pep 675][pep675]):

```by
type def Pad[X: literal int]:
    return Array[X.literal + 1]      # `X.literal` is `int`, no `None`
```

`literal int` is a single-valued subtype of `int`, so `Pad[4]` is fine, `Pad[int]`
is an ordinary invalid-type-argument error naming the bound, and `X.literal` is
typed `int` because the bound guarantees it. no new binder, no marshalling rule,
and the type form is useful well outside `type def` — `LiteralString` is exactly
its `str` case and already has to exist

the sugar of handing the body a bare `4` instead of a `TypeInfo` is then not
worth its cost. it would be a second way to spell a parameter, for a saving of
one attribute access, and it would put a value-shaped name in a type-parameter
list where no value lives

one caching consequence survives from the value framing, restated correctly: it
is not a parameter kind that hurts, it is the *question*. `X.literal` is a query
with unboundedly many answers, so a type function that asks it branches the
[decision tree](#caching-observation-traces) per distinct argument, where
`X <= int` branches it in two. `Pad` is memoized per size, which is fine for
sizes and bad for arbitrary strings — worth documenting so it is not discovered
as a mystery slowdown

### bounds on a deferred argument

a bound cannot be fully checked against an argument that is not ground:

```by
def f1() -> Pad[4]                     # `Literal[4]` — ground, runs now
def f2[Dim: int]() -> Pad[Dim + 4]     # deferred, nothing runs
```

`Dim + 4` is a symbolic operation whose reduced form is `int`. checking the
`literal int` bound against that reduced form would reject `f2`, which must be
legal — the whole point of [symbolic operations](../features/symbolic-type-ops.md)
is that `Dim + 4` folds to `Literal[7]` once `Dim` is `Literal[3]`. so the rule
is the one the existing machinery already implies:

- while the argument is symbolic, check only the part of the bound that its
    reduced form decides — `int` here, which `Dim + 4` satisfies
- single-valuedness is re-checked at specialization, when the operation folds and
    the argument becomes ground. `f2[3]` gives `Pad[Literal[7]]`, which runs; a
    specialization that folds to something non-literal is where the bound error is
    reported

that is a deferred precondition, and it means a violation of a `literal` bound
can surface at a call site rather than at the annotation. the
diagnostic has to carry both spans — the application that declared the bound and
the specialization that broke it — or it will be unreadable

### the rest of the parameter list

- defaults work as in [pep 696][pep696]: `type def F[X = int]` applies when the
    application omits the argument
- variance markers (`in`/`out`) are rejected on a `type def` parameter. a type
    function has no variance to declare — `F[A]` and `F[B]` are unrelated until
    both reduce — so accepting the syntax would be a lie
- [separators](../features/type-param-separators.md) (`/`, `*`) work, since the
    parameter list really is a parameter list

## when a type function runs

a type function runs when its arguments are **ground** — no type parameter,
no unsolved inference variable, anywhere inside them

when an argument is not ground the application stays symbolic:

```by
def push[T](xs: list[T], x: T) -> F[T]:  # `F[T]` is not evaluated here
    ...

push([1], 2)                             # `F[Literal[1] | Literal[2]]` → evaluated → int
```

this is exactly the existing [symbolic type operation](../features/symbolic-type-ops.md)
machinery: `DeferredType` already keeps `Dim + 1` symbolic until a specialization
substitutes `Dim`, and already re-runs the fold on substitution. a type-function
application is one more `DeferredOperation` variant, with two differences from the
existing kinds:

- its `reduced()` form cannot be computed by evaluating against upper bounds —
    running the function is the only way to know. instead `reduced()` is the
    function's **declared return type**:

    ```by
    type def F[X] -> int | str:      # unreduced `F[T]` behaves as `int | str`
    ```

    without a declared return the reduced form is `Unknown`. a strict-mode lint
    (`unannotated-type-function`) should require the annotation, because it is the
    only thing that makes generic code using `F[T]` checkable at all

- the fold is not a pure rust function but a call into the oracle (below)

keeping unreduced applications opaque is what preserves the decidability of
everything else in the type system. two unreduced applications are **equivalent**
iff they are the same function applied to equivalent arguments, and are otherwise
related only through their declared return types. there is no variance to infer:
an arbitrary function is neither co- nor contravariant, so `F[A]` and `F[B]` are
unrelated unless both reduce

## the `TypeInfo` api

`TypeInfo` is a frozen proxy for one type. it has two independent providers:

- the **static provider**, used when the checker calls the function: every
    question is answered by ty itself, over the wire
- the **runtime provider**, used when the same function is called by a running
    program (see [lowering](#lowering)): every question is answered by python
    introspection

one protocol, two implementations. writing the runtime provider is not optional
extra work — it is the thing that makes `F[X]` usable by `pydantic`,
`dataclasses` and `get_type_hints` after transpilation

the api mirrors the names already exposed by `ty_extensions._internal`, so there
is one vocabulary for static reflection in this project:

```by
class TypeInfo:
    # identity
    name: str | None                 # "int", None for structural types
    qualname: str | None             # "builtins.int"
    module: str | None
    kind: TypeKind                   # class | union | intersection | literal | callable | ...

    # relations
    def is_subtype_of(self, other: TypeLike) -> bool
    def is_assignable_to(self, other: TypeLike) -> bool
    def is_equivalent_to(self, other: TypeLike) -> bool
    def is_disjoint_from(self, other: TypeLike) -> bool
    def is_fully_static(self) -> bool

    # structure
    bases: tuple[TypeInfo, ...]
    mro: tuple[TypeInfo, ...]
    members: Mapping[str, TypeInfo]  # lazily faulted in, not eagerly shipped
    args: tuple[TypeInfo, ...]       # specialization arguments
    literal: object | None           # the value behind `Literal[...]`
    union_members: tuple[TypeInfo, ...] | None

    # construction
    def __or__(self, other: TypeLike) -> TypeInfo
    def __and__(self, other: TypeLike) -> TypeInfo
    def __invert__(self) -> TypeInfo          # `not` type
    def __getitem__(self, args) -> TypeInfo   # specialize

    @staticmethod
    def of(ref: str | type | object) -> TypeInfo
```

`TypeLike` is `TypeInfo | type | object`, so bare classes lift automatically and
`X <= int` works without ceremony

`TypeInfo.of("django.db.models.Model")` resolves **through ty's module
resolver**, not through a python import. a type function can name types that are
not importable in the worker's environment, which matters for stub-only packages

### operators

| spelling | meaning                    |
| -------- | -------------------------- |
| `x <= y` | `x` is a subtype of `y`    |
| `x < y`  | proper subtype             |
| `x == y` | equivalence (not identity) |
| `x \| y` | union type                 |
| `x & y`  | intersection type          |
| `~x`     | negation type              |

the sketch in the original proposal used `x < int` for "subtype". i would make
`<=` the one people reach for and keep `<` proper, because subtyping is
reflexive and `F[int]` hitting the `str` branch is a bad first experience.
the deeper trap is that subtyping is a **partial order**: `not (x <= int)` does
not mean `int <= x`, and a type function written as a two-way `if/else` on `<=`
is silently wrong for every unrelated type. the diagnostic story should include a
lint that flags a type function whose only test is a single `<=` with an `else`
branch — not because it is always wrong, but because it usually is

second trap: `<=` is **static subtyping**. `Any <= int` is false. authors who
mean "would this assignment be accepted" want `is_assignable_to`. i would keep
these separate and loudly documented rather than trying to pick a smart default

## execution engine

### shape

a persistent **worker pool**, not a process per call and not an embedded
interpreter:

- embedding CPython (pyo3) ties the checker to an abi, puts arbitrary user code
    inside the checker's address space, and makes a segfault in a type function
    take down the whole check. rejected
- one process per application costs 30–150ms of startup plus imports. rejected
- a pool of `min(4, cores)` long-lived workers, started lazily on the first
    ground application in a session, is the answer. steady-state cost per
    application is one round trip

each worker runs **the project's configured interpreter** (same resolution as
`by run`). if that interpreter's version disagrees with the configured
`python-version`, emit `type-function-environment-mismatch` once: the function
would be reasoning about a different stdlib than the one being checked

an *in-process sandboxed* interpreter would be better than any of these, and
would delete most of this section — see
[appendix a](#appendix-a-an-in-process-sandboxed-tier). nothing suitable is
stable today, so the design below assumes the worker pool and treats the
sandboxed tier as a later addition rather than a dependency

### transport

length-prefixed messagepack over a socketpair. **not stdout** — a `print()` in a
type function must not corrupt the protocol. stdout and stderr of the worker are
captured and surfaced as sub-diagnostics on the application site, which also
makes `print` a usable debugging tool inside a type function

messages:

```text
host → worker   Eval    { call, fn, args: [Handle] }
worker → host   Query   { call, op, operands: [Handle | Value] }
host → worker   Answer  { call, value }
worker → host   Result  { call, value: Handle | Diagnostic }
worker → host   Failure { call, traceback, stdout, stderr }
```

`Handle` is an opaque `u64` into a per-session slab mapping to `Type<'db>`. types
are never serialized whole: a `TypeInfo` carries only a shallow record (kind,
name, qualname, arity) and faults everything else in through `Query`, memoizing
per call. shipping a type eagerly is unbounded — recursive protocols, whole
member tables, the full mro of every base

`Query` is re-entrant: the host thread that dispatched `Eval` blocks in a service
loop answering queries with ordinary salsa queries. that re-entrancy is the one
place this design can deadlock, so the loop must be a plain loop on the
dispatching thread, never a hand-off to another executor

### cycles

a type function may ask about a type whose computation needs the same
application. the host keeps a stack of in-flight `(fn, args)` frames; a repeat is
`type-function-cycle`, a hard error with the frame chain as sub-diagnostics.
self-recursion on *different* arguments is legitimate and common —

```by
type def Flatten[X]:
    if X.name == "list":
        return Flatten[X.args[0]]
    return X
```

— so recursion is bounded by a configurable depth (default 100), not forbidden.
salsa fixpoint recovery is not applicable here: an arbitrary python function is
not a monotone operation on a lattice, so there is nothing to iterate to

### timeouts and failure

per-function timeout (`@type_fn(timeout=...)`, default 5s in the editor, 60s in
CI). on timeout the worker is killed and replaced, the application becomes the
declared return type, and `type-function-timeout` is reported. a worker that dies
never fails the whole check — one bad function degrades one type

## caching: observation traces

this is the part that makes the feature viable rather than a curiosity

a naive cache keyed by `(function, arguments)` is both too slow and **wrong**:

- too slow, because `F[X]` for two hundred distinct `X` is two hundred process
    round trips, when the function may only ever ask one question
- wrong, because the function observed more than its arguments. if it read
    `SomeClass.mro` and the user edits `SomeClass`, the arguments are unchanged and
    the cached answer is stale. under salsa this is a correctness bug, not a
    staleness annoyance

so cache the **observations**, not the arguments. every `Query` the worker made
is recorded in order, with its answer. the recorded sequence is a path through a
decision tree, because control flow in the body is a function of the answers.
accumulating those paths per type function gives one decision tree per `type def`:

```text
F:
  is_subtype_of(arg0, int) ?
    ├── true  → int
    └── false → is_subtype_of(arg0, str) ?
                  ├── true  → str
                  └── false → <not yet explored, run the worker>
```

evaluating `F[bool]` becomes: walk the tree, answering each node with the
ordinary ty query for the *new* arguments. a leaf is a hit — no ipc at all, cost
is a handful of already-memoized subtype checks. falling off the tree dispatches
the worker and grows a branch

three things fall out of this for free:

1. **correct incrementality.** each node is an ordinary salsa query. editing a
    class the function observed changes that node's answer, the walk takes a
    different path, and the result changes — automatically, with no invalidation
    logic of its own. the only non-salsa input is the leaf, which is stored as a
    salsa input keyed by the path
1. **collapse.** hundreds of arguments share a handful of leaves. a type function
    is usually a small decision procedure over a few relations, and the tree is
    exactly that procedure, extracted by observation
1. **explanations.** the path *is* the reason. `by explain F[bool]` printing
    "`bool <= int` → returned `int`" is a projection of the cached path, not a
    feature that has to be built separately. this should ship with the feature;
    the alternative is a language where nobody can answer "why is my type that"

the correctness proviso is the author's footgun, and should be stated plainly in
the user docs: replaying a trace is sound only if the function is deterministic
with respect to everything the trace does not capture. a function that reads a
file is not, and gets an explicit cache policy:

```by
@type_fn(cache="persist")   # default: tree persisted to `.by_cache/type-fns/`
@type_fn(cache="session")   # tree kept in memory, rebuilt each process
@type_fn(cache="never")     # re-executed per application, no tree
```

`cache="never"` is honest but should be rare, and the editor should refuse to run
`never` functions on every keystroke — debounce or degrade to the declared return

### determinism auditing

- `by check --verify-type-functions` re-executes every leaf and compares,
    reporting `type-function-nondeterministic` on a mismatch
- a `type-functions.lock` recording `fingerprint → result` for impure functions,
    committed like a package lock, so a network-reading type function cannot make
    CI green and a laptop red. this is optional but i think it is what makes impure
    type functions defensible in a team setting rather than merely permitted

## errors

three distinct outcomes, three distinct diagnostics:

```by
type def Field[X]:
    if not X.is_hashable():
        return TypeError(f"{X.name} cannot be a field: not hashable")
    return X
```

| outcome                 | diagnostic                                                         | resulting type                    |
| ----------------------- | ------------------------------------------------------------------ | --------------------------------- |
| `return TypeError(msg)` | `invalid-type-argument`, message verbatim, span on the application | `TypeError.fallback` or `Unknown` |
| uncaught exception      | `type-function-crashed`, python traceback as sub-diagnostic        | declared return type              |
| timeout / worker death  | `type-function-timeout`                                            | declared return type              |
| non-type return value   | `invalid-type-function-return`, with `repr`                        | `Unknown`                         |

returning an error rather than raising is the right primitive: it is a value, so
it composes, and it leaves `raise` to mean "this type function has a bug", which
is a genuinely different situation deserving a different message. `TypeError`
should carry an optional fallback so one rejected argument does not cascade:

```by
return TypeError("not hashable", fallback=object)
```

a type function should also be able to report without failing:

```by
report(Warning("this shape is deprecated"))
return int
```

`report` accumulates into the result envelope and is emitted at the application
site. it is deliberately not a `ctx` parameter — that would leak into the
desugared signature and into the runtime provider

## type-system integration

| position                                           | rule                                                                                                                                    |
| -------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| annotation                                         | evaluated if ground, deferred otherwise                                                                                                 |
| base class                                         | ground only. `class C[T](F[T])` is rejected — an mro cannot be deferred                                                                 |
| parameter, argument mentions an inference variable | solve the variable from other positions first, then reduce. if it is only solvable *through* `F`, report `type-function-not-invertible` |
| `isinstance` / narrowing target                    | reduce first; a deferred application narrows to its declared return                                                                     |
| protocol member                                    | deferred like any other annotation                                                                                                      |
| `type` alias rhs                                   | evaluated when the alias is used, not when declared                                                                                     |

the non-invertibility rule is the one that will bite. `def f[T](x: F[T]) -> T`
cannot be solved, because inverting an arbitrary function is not a thing. saying
so with a dedicated diagnostic is much better than silently inferring `Unknown`

## note: contextual application (`Expected`)

not proposed, recorded because it is the first thing people ask for once type
functions exist, and because the answer is not "add a feature" but "pick a side
of an asymmetry"

the ask is for a type function to see the **expected type** at the use site:

```by
def loads(s: str) -> Parsed: ...

a: MyModel = loads(text)     # can `Parsed` learn `MyModel`?
```

it cannot today, and not because of a missing hook. inference flows *forward*
through an application while context flows *backward*, and an application is
opaque, so the two never meet:

```by
type def Id[X]:
    return X

def direct[E](x: E) -> Id[E]       # `direct(1)` is `1`
def through[E](x: Id[E]) -> E      # `through(1)` is `Unknown`
```

you can solve **into** an application — pin the argument from somewhere else,
then the deferral folds — but never **through** one, which is the
non-invertibility rule above seen from the other end. so the expected type can
never be *solved for*; the checker would have to *supply* it

### a spelling

the expectation reads better as a type form than as a new binder:

```by
type def Parsed[X]:
    if X.name == "set":
        return TypeError("cannot deserialize a set")
    return X

def loads(s: str) -> Parsed[Expected]: ...
```

`Expected` needs no new binder, composes with everything (`list[Expected]`), and
puts the dependence in the *consumer's* signature where the reader is, rather
than hiding it on the `type def`. it also subsumes the `def f[T]() -> T` idiom
and makes it honest — `-> Expected` says exactly what that annotation means

### what it would cost

- **a signature stops being fixed.** today a return annotation is evaluated once
    when the signature is built; this makes the return type a function of the call
    site. that is a change to how signatures are cached, not a change to type
    functions. the shape fits `DeferredOperation::TypeFn` — one operand becomes a
    context placeholder filled during call inference — but the plumbing is
    upstream of this feature
- **"expected" is often absent.** `f()` as a statement, or `x = f()` with no
    annotation. a [pep 696][pep696] default (`type def Parsed[X = object]`) is a
    better answer than inventing a sentinel
- **`a: int | str = f()`** hands the function a union rather than a choice, and
    authors will expect it to distribute
- **overloads are circular**: the context picks the overload whose return type is
    the type function that wants the context. either evaluate after selection with
    the context selection already used, or refuse context-sensitive returns in
    overloads
- **diagnostics anchor at the call site**, which promotes the "a re-fold reports
    nothing" gap from incidental to blocking — the whole value is a `TypeError`
    reaching the user
- **lowering has no answer.** there is no single type to emit for the
    declaration, and `__annotations__` genuinely cannot express a per-call-site
    type, so `get_type_hints` consumers see only the fallback

### why it would be worth anything

`a: int = f()` and `b: str = f()` getting different types out of one function
*is* the unsoundness of `def f[T]() -> T`, which the language already permits.
the difference is that a type function can refuse. so this is not sugar: it
converts a silently-unsound idiom into one with a checked contract. that is the
argument for it; convenience is not

the model is well understood elsewhere — haskell's `read :: Read a => String -> a`
is return-position polymorphism resolved by expectation, with a class constraint
that can fail. the type function plays the constraint's part

### the cheaper 80%

[explicit type application](../features/reified-generics.md) already exists, and
covers the checkable-contract case with no context plumbing, no cycles and no
per-call-site signatures:

```by
def loads[T](s: str) -> Parsed[T]: ...

a = loads[MyModel](text)     # validates the target
b = loads[set[int]](text)    # error: cannot deserialize a set
```

the cost is naming the type once, where an annotation would have named it anyway.
worth shipping first and seeing whether the repetition actually hurts before
paying for `Expected`

## lowering

two paths, chosen per application site:

**ground applications are inlined.** the checker already computed the answer, so
the transpiler consumes the same cache and writes the result:

```by
def f() -> F[bool]
```

```python
def f() -> int: ...
```

no runtime cost, no runtime dependency, and the emitted python is what a human
would have written. this is only possible because check and transpile share one
oracle cache — worth designing for from the start rather than retrofitting

**non-ground applications keep a runtime object.** `F` lowers to a real function
plus a marker:

```python
@_by_type_fn
def F(X):
    if X.is_subtype_of(int):
        return int
    return str
```

`_by_type_fn` wraps it in an object whose `__getitem__` builds runtime `TypeInfo`
values via the runtime provider, so `F[T]` inside a generic function still means
something at runtime for `get_type_hints`-driven frameworks. this needs a real
runtime package rather than a preamble — the `TypeInfo` runtime provider is far
too large to inject as generated lines, and it is the natural first inhabitant of
the `bython` runtime package the roadmap already anticipates

reverse transform detects `@_by_type_fn` and restores `type def`. inlined
applications are unrecoverable by construction, which is the usual sourcemap
problem and not new here

`--type-fn-lowering=runtime` forces the second path everywhere, for authors who
want the transpiled output to keep evaluating

## trust

checking a project must not silently execute code that came from a dependency.
this is a supply-chain surface, and the fact that the author owns the footgun for
*their own* type functions does not extend to someone else's

this whole section exists because the executing code has ambient authority. it is
the part of the design that a [sandboxed tier](#appendix-a-an-in-process-sandboxed-tier)
would delete outright

- first-party type functions (inside the project root) execute by default

- third-party ones do not, until listed:

    ```toml
    [tool.basedpython.type-functions]
    trust = ["some_package"]
    ```

    an untrusted application degrades to the declared return type and reports
    `untrusted-type-function` once per module

- `--no-type-functions` disables execution entirely; every application becomes
    its declared return. CI on untrusted pull requests should use it, and it should
    be the default in any "check this snippet" web surface

note the module-level consequence: the worker **imports the defining module** to
get at the function, so that module's import side effects run. keeping type
functions in cheap-to-import modules is a documented recommendation, and
[lazy imports](../features/lazy-imports.md) already help

## configuration

```toml
[tool.basedpython.type-functions]
enabled = true
trust = []
workers = 4
timeout = "5s"
recursion-limit = 100
cache = ".by_cache/type-fns"
```

## staging

1. deferred application type + declared-return reduction, `--no-type-functions`
    semantics only. no execution at all. this alone is a shippable, useful,
    completely safe subset, and it de-risks every type-system integration point
    before any process management exists
1. worker pool, protocol, handle slab, static `TypeInfo` provider, flat memo
    keyed by argument fingerprints. correctness of incrementality is knowingly
    wrong here; gate behind a preview flag
1. observation traces and the decision tree, replacing the flat memo. this is
    where it becomes correct and fast
1. `by explain`, error taxonomy, trust config
1. runtime provider + `@_by_type_fn` lowering
1. determinism auditing and the lockfile

put the execution engine behind an **oracle interface** from step 2, even though
there is only one backend. it is a small discipline that keeps
[appendix a](#appendix-a-an-in-process-sandboxed-tier) reachable later without a
rewrite, and it is the natural seam for `--no-type-functions` anyway

## thoughts

**the roadmap's `match` types are the more important feature.** `0.0.1a8`
already plans

```by
type Nested[T, *Shape: int] = match *Shape: ...
```

that declarative form covers most of what people would reach for `type def` to
do, and it is statically analyzable: no worker, no trust problem, no
nondeterminism, no cache, and it works in untrusted code and in the browser. i
would ship `match` types as the primary mechanism and `type def` as the
unrestricted escape hatch, with both compiling to the same deferred-application
machinery. designing `match` types as sugar over `type def` would be the wrong
way round — it would drag execution into cases that never needed it

**turing-completeness is the point, and containment is the design.** allowing
arbitrary computation is fine precisely because unreduced applications are
opaque: subtyping never has to run a type function to answer a question, it only
has to compare applications syntactically. the moment something tries to be
clever — say, deciding `F[A] <= F[B]` by probing `F` on samples — decidability
and cache soundness both go

**the explanation surface is not optional.** every type-level computation feature
in every language collapses under "why is my type that". this design gets
explanations free from the trace cache, and hover-on-`F[bool]` showing the path
should be treated as part of the feature, not a follow-up

**almost every cost in this document is the price of ambient authority.** the
brief is that a type function may do anything and the author owns the footgun,
and that is what the design delivers. it is worth being clear-eyed about what it
costs, because the bill lands on every type function, not just the ones that
spend the freedom: the trust configuration, the lockfile, the "deterministic
outside the oracle" proviso under the trace cache, worker lifecycle, and the
entire ipc protocol all exist for it. a type function that only asks `X <= int`
pays all of that and uses none of it. that asymmetry is the argument for
[appendix a](#appendix-a-an-in-process-sandboxed-tier) later, and the reason the
oracle should be an interface from the start

**impurity is really about reproducibility, not safety.** nobody is much harmed
by a type function reading a file. what harms people is a type function that
resolves differently on two machines, because the resulting error appears to be
in code that never changed. the lockfile is the mitigation and i think it should
exist before impure functions are advertised

**validation is a distinct use case worth naming.** a large fraction of real
usage will be `type def` that returns its argument unchanged or a `TypeError` —
a constraint, not a function. it may deserve its own spelling (`type check`?) so
that the common case does not have to end in `return X`, and so the checker knows
the result is the argument and can skip the deferral entirely

**things that will go wrong, concretely**: a `print` corrupting the protocol
(hence the socketpair), `sys.exit` in a function body, threads or asyncio left
running in the worker, `atexit` handlers firing on worker recycle, a type
function importing the module currently being checked, unbounded handle-slab
growth over a long editor session, and a stack overflow from mutual recursion
between two type functions that individually look bounded. each of these is
cheap to handle at design time and expensive to retrofit

## what is implemented

the feature works end to end — `by check`, `by transpile` and `by run` all handle
type functions:

- **syntax.** `type def F[X]:` parses to a `StmtFunctionDef` carrying a synthetic
    `type_fn` marker decorator, like `extension`/`enum`. every consumer agrees on
    the spelling through one predicate (`ruff_python_ast::helpers::is_type_def`)
    rather than matching the string itself
- **application.** `F[bool]` in a type expression applies the type function. the
    body is executed by `python3`, with each argument described *eagerly* as a
    self-contained python object: name, mro, literal value, union members. so
    `X <= int`, `X.name`, `X.literal`, `X | None` and `return TypeError(...)` all
    work
- **results are exact, and compose.** the answer is a small graph of nodes rather
    than a name, so a composed form survives whole: `list[X]` is a generic node
    whose argument is the *handle* of the application's own argument, which means
    the specialization is exact rather than reconstructed from a spelling that
    would have lost it. a returned class object is resolved by qualified name
    through ty's module resolver, without importing anything
- **any type form may be returned.** classes, `Literal[...]`, generic aliases
    (`list[int]`, `tuple[int, str]`), unions (`X | None`, `Union[...]`,
    `Optional[X]`), and the type function's own arguments. a **bare value is its
    literal type** — `return 1` is `Literal[1]`, matching how `1` reads in an
    ordinary type position, so the explicit `Literal[...]` is never required
- **memoization.** evaluation is a Salsa query keyed on the function and its
    interned arguments. because the argument description is built *inside* the
    query, editing a class a type function looked at invalidates the result the
    ordinary way
- **deferral.** an application whose arguments still mention a type parameter
    stays symbolic as a `DeferredOperation::TypeFn`, behaves as the declared
    return type meanwhile, and re-runs when a specialization substitutes the
    argument
- **preconditions.** arity is checked first, then type-parameter bounds — both
    before any interpreter starts, so a mistake is a diagnostic rather than a
    python traceback
- **errors.** a returned type; `TypeError` (the author's message verbatim); a
    crash (the python traceback). a failed application degrades to the declared
    return type, so one bad application does not cascade
- **isolation.** the body's own stdout is redirected to stderr and the result
    travels on a sentinel-prefixed line, so a `print` in a type function is a
    usable debugging tool rather than protocol corruption. at most four
    interpreters run at once, each killed after 10s
- **lowering.** a ground application is *inlined* to the type it evaluated to and
    the declaration is erased, leaving no trace in the emitted python. an
    application that stayed deferred lowers to its declared return, or to `Any`
    when there is none — never to the *display* of `Unknown`, which is not a
    runtime name. naming a `type def` in a value position is rejected in either
    spelling (`F` or `F[int]`), since the erased declaration would raise
    `NameError`
- **round-trip.** `buff format` preserves `type def` exactly

### trust

executing a type function runs arbitrary code with the checker's authority, so
*checking* a project must never run code that came from somewhere else:

- only **first-party** type functions execute. one from site-packages, a vendored
    stub, or the standard library is refused and degrades to the declared return
- **`BY_NO_TYPE_FUNCTIONS=1`** disables execution entirely — for CI on an
    untrusted pull request, or any "check this snippet" surface

this is the environment-variable form of the [trust](#trust) design above; the
per-package allowlist and a real CLI flag are still to come.

### what is not done

- **the lazy oracle.** arguments are described eagerly instead of proxying back
    into ty, so relations are **nominal only**: protocols and structural
    subtyping are invisible to `X <= ...`, and `members`/`args` are empty. this is
    the largest remaining gap and the reason for the [transport](#transport)
    section
- **the body executes on its own.** it is reconstructed from the source text of
    the `type def` alone, so it sees builtins and nothing else — a name from the
    enclosing module must be imported *inside* the body. the design's worker
    imports the defining module instead, which is what would close this
- **the body is not type-checked.** two things must be modelled first, and they
    are the same knot: a type function's parameters are the type arguments of an
    application rather than the `TypeInfo` values its body receives, and the arrow
    in `type def F[X] -> int | str` declares the *resulting type* while the body
    `return int` returns a **class object**. so the annotation cannot be checked as
    an ordinary value-level return — the body would have to be checked against
    something like `TypeForm[int | str] | TypeError`. until then the marker also
    sets `NO_TYPE_CHECK`; the separate `TYPE_FN` flag is what keeps a type def
    distinguishable from an ordinary `@no_type_check` function
- **impurity is cached.** evaluation is a Salsa query, so a type function that
    reads the network or a file has its result frozen for the revision. the design's
    per-function cache policies (`cache="never"`/`"session"`) do not exist yet, and
    the trust gate above is what currently bounds the damage
- **a re-fold reports nothing.** when a deferred application re-runs on
    specialization there is no diagnostic sink, so a `TypeError` raised only for a
    particular specialization degrades silently to the declared return
- **`python3` must be on `PATH`** — including for the mdtest suite. there is no
    interpreter discovery and no graceful skip
- **worker pool, `by explain`, per-package trust, and the runtime-object
    lowering** for non-ground applications (which currently lower to the declared
    return rather than staying live)

files: `crates/ty_python_semantic/src/types/type_fn.rs` (evaluation, the
python-side `TypeInfo`, trust, arity and bounds), `types/deferred.rs`
(`DeferredOperation::TypeFn`), the marker in `ruff_python_ast/src/helpers.rs` and
`ruff_python_parser/src/parser/statement.rs`, the application hook in
`types/infer/builder/type_expression.rs`, the value-position rejection in
`types/infer/builder/subscript.rs`, `ruff_python_formatter/src/statement/stmt_function_def.rs`,
`crates/by_transforms/src/transforms/type_fn.rs` (erasure) with the inlining in
`transforms/symbolic_type_op.rs`, and `resources/mdtest/basedpython_type_def.md`

## appendix a: an in-process sandboxed tier

not part of the proposal — parked until something suitable is stable. recorded
because it changes enough of the design that it is worth knowing in advance which
parts are load-bearing and which are only there to service a subprocess

the shape would be **two tiers**: a sandboxed interpreter as the default, and the
CPython worker pool for functions that declare they need the world

```by
@type_fn                         # sandboxed, deterministic, in-process
type def F[X]: ...

@type_fn(runtime="python")       # CPython worker, unrestricted, opt-in
type def Schema[X]: ...
```

the tier would be declared, never inferred. inferring it from whether the body
happens to import `requests` would let a one-line edit move a function from
provably deterministic to arbitrary authority in silence

what it would buy, in descending order of value:

- **the transport disappears.** the reflection api becomes host functions calling
    ty directly, on `Type<'db>` values that never leave the process. no
    socketpair, no msgpack, no handle slab, no re-entrant service loop, no
    deadlock risk, no stdout-corruption trap. the speed is incidental; deleting
    the most error-prone third of this document is the point
- **trace replay becomes provably sound.** the [decision-tree
    cache](#caching-observation-traces) is sound only if the function is
    deterministic with respect to everything the trace does not capture. with no
    ambient authority that holds by construction — the oracle is the only input —
    so the proviso stops being a documented hope
- **the trust problem dissolves for the default tier**, because a sandboxed
    function has nothing to be trusted with. [trust](#trust) narrows to the
    functions that opted out of the sandbox
- **it works where a process pool cannot** — a wasm build would let a browser
    playground or a locked-down CI evaluate type functions at all

what to check before adopting one:

- **does it support what the api needs.** the [`TypeInfo` api](#the-typeinfo-api)
    is a class with operator overloads. the questions are whether a
    *host-provided* opaque object can carry `__le__`/`__or__` in that
    interpreter, and whether its language subset is enough to write a real type
    function in. if host objects cannot carry dunders, the api falls back to free
    functions — `is_subtype_of(X, int)`, exactly the shape
    `ty_extensions._internal` already uses — which is survivable, but the
    `X <= int` surface is not
- **two dialects.** a function under the sandbox and a function under CPython are
    not the same language. an unsupported construct must be a clean error naming
    the tier, never a silent escalation to the unrestricted one
- **maturity.** this is a dependency on someone else's interpreter for the
    correctness of type checking, which is a heavier commitment than a normal
    crate

[pydantic's monty][monty] is the current candidate — a python interpreter in
rust, distributed as a crate, sandboxed by design, and it already embeds `ty`.
as of this writing its readme calls it experimental and "not ready for prime
time", and it does not yet support classes, so it is parked rather than adopted

[monty]: https://github.com/pydantic/monty
[pep675]: https://peps.python.org/pep-0675/
[pep696]: https://peps.python.org/pep-0696/
