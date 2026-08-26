# semantics, testing, and phasing

## semantic deltas

compiled and interpreted basedpython must agree. where they cannot, the
difference is listed here, and anything not listed here is a bug

python is a smaller problem for us than it is for mypyc, because basedpython has
already ruled out most of what makes compilation observable. the table below is
short for that reason:

| behaviour                                   | interpreted                          | compiled                          | why                                                               |
| ------------------------------------------- | ------------------------------------ | --------------------------------- | ----------------------------------------------------------------- |
| monkey-patching a module function           | works                                | tier 3: no                        | early binding, gated on `api.lock`                                |
| adding an attribute not in the class body   | stored, unless `__slots__`           | the same, on 3.13 and above       | an instance dict beside the layout; below 3.13 it is refused      |
| `is` on a fixed-length tuple                | identity                             | unspecified                       | unboxed tuples have no identity                                   |
| `type(x)` where `x: int` held a `bool`      | `bool`                               | `int`                             | the tagged form has no room for it                                |
| `is` on a small `int` or interned `str`     | identity                             | unspecified                       | unboxed and re-boxed values differ                                |
| a wrong annotation on non-compiled code     | `TypeError` (soundness checks)       | `TypeError`                       | same mechanism, already shipped                                   |
| `__dict__` on an instance                   | present unless `__slots__`           | absent where the class has fields | a mapping naming only the dict half would be a quiet wrong answer |
| `if __name__ == "__main__"`                 | n/a                                  | n/a                               | the entry point is [`main`](../../features/main-function.md)      |
| stack depth on deep recursion               | python's limit                       | the C stack's                     | native frames are not python frames                               |
| `type(f)` for a compiled function           | `function`                           | `builtin_function_or_method`      | a native function is a C function object                          |
| `f.__code__`, `f.__defaults__`              | present                              | absent                            | there is no python code object behind it                          |
| a `data class` constructor's argument types | unchecked (unless `--soundness all`) | checked                           | a compiled field is unboxed, so the check is mandatory            |

the three `is`-related rows are the only genuine losses, and they are the same
ones mypyc takes. `is` on immutable value types is already a python anti-pattern;
`buff` should grow a lint for it under a compiled configuration rather than
leaving it to a runtime surprise

the constructor row is the representation invariant doing its job: a `data class`
field with an `int` annotation is an unboxed `ByTagged`, and a `str` cannot be
stored there at all. so the check is not a choice — it is the `parameters`
soundness position, mandatory wherever a field is unboxed. the interpreted build
reaches the same behaviour under `--soundness all`

the two function-object rows are the price of the calling
convention rather than of any optimization: a natively compiled function is a
`PyCFunction`, so it has no `__code__` and introspection that reaches for one
fails. a declined function keeps its python function object, so the fallback is
also the escape hatch for code that needs introspection

the `bool` row gets an opt-out once
[`final T`](planned-features.md#final-t-exactness-at-the-use-site) lands: a
`final int` place rejects `True` at the checker, so the divergence is
unobservable there. the row stays, but it stops being unavoidable

### what refuses to compile

- a module whose check has **errors**. this is already `by run`'s rule: the
    checker's verdict and the runtime must not diverge
- tier 3 with a stale `api.lock`
- `--soundness none` together with `--tier=3` without an explicit
    `--i-know-what-this-means`, because that combination removes both the
    runtime guard and the closed-world escape hatch at once

## coverage escape hatches

there are two, at different granularities, and between them the language is
fully covered from the first milestone:

**per function** — a function containing a construct with no native lowering
becomes an [interpreted fallback](runtime.md#boxed-classes-and-interpreted-fallbacks).
the module's whole transpiled python is embedded in the extension and executed in
the module's own namespace from a `Py_mod_exec` slot, and the natively compiled
functions are then installed *over the top* with `PyModule_AddFunctions`. so
module-level code runs, declined functions exist, and the natives win. it is slow
in that one place and correct everywhere

the ordering is the whole trick: `m_methods` has to stay `NULL`, because methods
named there are installed when the module object is created — before the exec
slot runs — and the interpreted definitions would then overwrite them

**per module** — a module the compiler declines entirely is simply left as `.py`
and imported normally. compiled and interpreted modules interoperate, so this
costs nothing but speed in that module

both emit a diagnostic under `by compile --verbose`, naming the construct and
the reason. the count of fallbacks in a project is the honest measure of how far
along the compiler is, and it should be reported

### `--no-any`

`by compile --no-any` reports every place in a module where a gradual type enters
a compiled signature, and fails the build

it reads the *types*, not the outcome. that is not an implementation detail: once
`object` became a real representation, a gradual type stopped declining — it
compiles, just through the abstract object protocol — so there was no longer any
decline to look at. the mode had to move from "what failed to compile" to "what
is gradual", which is the question it was always really asking

`--require-native` is the stricter sibling: it rejects *any* decline, including
one caused by a perfectly precise type the compiler does not represent yet. the
two ask different questions — "is this fully typed" and "does this fully
compile" — and a module can satisfy the first and fail the second

`--no-any` is a **predictability contract**, not a speed switch, in the same sense as
[`overlapping`](planned-features.md#overlapping-t-a-guarantee-not-an-optimization):
it does not make code faster, it stops code from silently becoming slower. the
performance it *licenses* arrives later — a module with no gradual values has no
gradual → typed edges, so once the boxed `object` representation lands there is
nothing in it for the internal soundness checks to check

## differential testing

this is the backbone, and it is mostly already built

`crates/ty/tests/mdtest_divergence.rs` takes every `.by` code block in the
mdtest suite that the checker accepts, transpiles it, and *executes* it,
catching "checks clean but crashes at runtime". compilation adds a third leg to
the same harness:

```text
  .by block
     ├─ ty check          → must be clean          (mdtest, today)
     ├─ transpile + run   → must exit 0            (mdtest_divergence, today)
     └─ compile + run     → must produce byte-identical stdout, stderr, and exit code
```

this gives the compiler several hundred correctness cases on day one, growing
with the language, with no new test corpus to write. every basedpython feature
that lands with an mdtest automatically constrains the compiler too

the other layers:

| layer              | form                                                                                                          |
| ------------------ | ------------------------------------------------------------------------------------------------------------- |
| IR snapshots       | `.by` in, textual BIR out, as mdtest-style markdown files                                                     |
| the verifier       | runs after every pass in debug builds; a failure is a panic in tests, never in a release build                |
| runtime unit tests | C tests for `by_rt`, plus ordinary rust tests for the rust half                                               |
| ABI tests          | compiled module ↔ interpreted module in both directions, including subclassing, pickling, and traceback shape |
| sanitizers         | the full suite under asan and ubsan in CI                                                                     |
| property tests     | generated `.by` programs, run both ways, compared                                                             |
| benchmarks         | tracked per-commit, against interpreted basedpython, cpython, and mypyc on the same source                    |

the property tester is worth building early rather than late. a generator over
the *typed* subset of the language — which we can drive from ty's own type
lattice — finds representation bugs that hand-written tests do not, and
representation bugs are the ones that corrupt memory

### shared-constant invariants

two numbers will end up living in both the checker and the compiler, and a test
has to pin them together rather than trusting a comment:

- the [`single`](planned-features.md#single-t-declared-discriminated-unions-for-generics)
    decomposition member cutoff. if the checker decomposes where the compiler
    declines, a program type-checks against a shape the compiler cannot
    represent, and the failure surfaces at codegen instead of at the declaration
- `compile.monomorphize-limit`, once `literal` adds value keys to the same budget
    that type keys already draw on

both should be a single `pub const` with a test asserting each consumer reads it,
in the spirit of the existing `MAX_EXACT_TUPLE_PATTERN_ELEMENTS`

## milestones

each milestone ends with something runnable and benchmarked. no milestone is
"the optimizer"

**M0 — skeleton.** `by_ir`, the verifier, the BIR printer, and an IR snapshot
test harness. `by compile` on a module containing one function that returns a
constant, producing a loadable `.so`. no optimizations at all
*exit: `python -c "import m; print(m.f())"` works, and CI builds the extension on
linux, macos, and windows*

**M1 — the interpreted-fallback compiler.** every construct falls back; only
straight-line integer and float arithmetic, comparisons, `if`, `while`, `for` over
`range`, and calls between compiled functions are native. tagged ints, unboxed
doubles, the `python` wrapper, the refcount pass, exception edges
*exit: the differential harness passes on the whole mdtest corpus; a numeric
benchmark beats cpython*

**M2 — native classes.** fixed layouts, vtables, traits from `protocol class`,
`data class` always-defined attributes, direct field access, `final`
devirtualization
*exit: an object-heavy benchmark beats cpython by ≥3×*

**M3 — containers and strings.** `list`, `dict`, `set`, `str`, `bytes` fast paths,
unboxed fixed tuples, comprehensions, iteration protocols, the grapheme
intrinsics
*exit: fallback count on a real project reaches zero for ordinary code*

**M4 — the basedpython optimizations, first half.** `raises`-driven error-path
elision, `sealed` and `enum class` tagged unions, `local` borrowed arguments,
`final` and closed-world devirtualization
*exit: measurable win on each, individually, in the benchmark suite; each has a
`--annotate` explanation*

**M5 — the second half.** escape-driven stack allocation, monomorphization,
range analysis, inline blocks, the `api.lock` tier-3 boundary, LTO
*exit: a tier-3 build of a real basedpython project, with the lockfile check
wired into CI*

**M6 — distribution and ergonomics.** the PEP 517 backend, the setuptools hook,
`by run --compiled`, `--annotate`, `#line` and traceback integration, incremental
rebuild timings
*exit: `pip install .` on a basedpython project produces a working wheel, and a
one-function edit rebuilds in under a second*

### measured

against mypyc on the same source — a mandelbrot render, ordinary python, python
3.13:

|        | cpython | mypyc      | here   |
| ------ | ------- | ---------- | ------ |
| mandel | 39.4ms  | **0.86ms** | 1.31ms |

**24x over cpython**, and within 1.5x of mypyc. the float kernel compiles to seven
plain double instructions per iteration, the same as mypyc's; what is left is the
tagged integer counter, which both compilers still carry — mypyc simply spends
fewer instructions on it

on array work the buffer wins outright: `dot` 6.8ms against mypyc's 9.7ms, and
`prefix` 0.7ms against 3.5ms, which is hand-written C speed. mypyc's erased
generics make its `list[float]` a `PyListObject` of boxed floats, so it cannot
reach that shape at all

### what has shipped

M0, M1 and M2 are done. what exists:

- **ordinary python** — `by compile` lowers a `.py` file's AST the same way it
    lowers a `.by` one, and a `.py` source is its *own* interpreted fallback: it is
    already the program, and transpiling it would be a round trip through a
    different one. the source language is a single value, `by_irbuild::Language`, and it
    settles how the source parses, whether a loop's binding is fresh on each
    iteration, and where a declined function's definition comes from

- **representations** — tagged `int`, unboxed `double`, `bool`/`bit`, `str`,
    `list`/`dict`/`tuple`, fixed tuples as structs, `object`, and an instance
    pointer per emitted class

- **statements and expressions** — every operator, `if`/`while`/`for`, `break`,
    `continue`, loop-`else`, `try`/`except`/`finally`, f-strings, all four display
    forms, comprehensions, subscripting, decorators on module-level functions

- **python's numeric tower** — a mixed `int`/`float` pair is a double operation.
    python converts the `int` side by a rounding conversion before operating, which
    is exactly what `Op::IntToFloat` emits, so the double op *is* the operation —
    right down to the `OverflowError` an integer with no float at all raises

- **the promoting `float` annotation** — a `.py` parameter written `float` means
    `int | float`, so a `double` is not a promise the caller has to keep. the body
    compiles against one anyway and the *boundary* tests each call: an argument that
    is not exactly a float is handed to the interpreted definition, which keeps an
    `int` an `int` all the way through. rejecting it would be wrong, and converting
    it would be a different program

- **`match`** — every pattern shape: value, singleton, capture, wildcard, `as`,
    `|`, basedpython's `and`, guards, sequence (fixed and starred), class
    (positional and keyword) and mapping patterns. a case
    is a *test* and a set of *bindings*, kept apart because python binds before it
    evaluates a guard and leaves the binding behind when the guard fails; the
    bindings happen as the test goes, so a sequence element is read once. a
    singleton is an **identity** test, so `0` does not match `case False:`, and a
    missing attribute in a class pattern is *no match* rather than an error

- **generics** — a type parameter is erased at runtime, so a generic function or
    method needed nothing but the decline coming off. what is not erased is the
    namespace: `Box[int]` keeps working because the emitted type answers
    `__class_getitem__` itself

- **dunder slots** — `__repr__`, `__str__`, `__len__`, `__bool__`, `__hash__`, the
    six comparisons through one `tp_richcompare`, and the arithmetic pairs through
    `nb_add` and friends. a class defining `__eq__` without `__hash__` becomes
    unhashable, which `PyType_FromSpec` does not do for itself

- **evaluated once, copied across** — a computed parameter default, and a
    class-level constant, both come from the *interpreted definition* that already
    evaluated them at definition time rather than being computed a second time.
    that is what makes a mutable default shared by every call that omits it, and it
    keeps the object identical between the two halves

- **plain classes** — a hand-written `__init__` is lowered as a method and `tp_init`
    binds against *its* signature; the layout is the attributes that constructor
    assigns at its top level. an attribute assigned anywhere else declines, because a
    struct field has no way to be absent where python would raise `AttributeError`

- **dunder slots** — `__repr__`, `__str__`, `__len__` and `__bool__` are installed
    both as ordinary methods and as type slots, because `repr(x)` reads `tp_repr` and
    never looks the name up. each adapter calls the method's own wrapper, so the
    binding and the boxing are the ones every other call gets

- **native classes** — a `data class` gets a fixed struct layout, a static
    `PyTypeObject`, generated `tp_init`/`tp_dealloc`, getters and (unless `frozen`)
    checked setters. fields may have literal defaults, the constructor takes them
    positionally or by keyword, a class may have no fields at all, may carry
    decorators, and may extend another class this module emits. an attribute is a field read at a compile-time offset, and a
    method call is **direct** — nothing can subclass an emitted class, so there is
    nothing for a vtable to dispatch on

- **closures and lambdas** — a nested function is a method of a generated
    environment class, so a captured read is a field read; a closure the frame made
    itself is called at its native entry point. a name either frame *writes* becomes a
    shared cell, so python's close-over-the-variable semantics hold. a lambda is a
    synthesized nested function and needs no second code path. nesting goes to any
    depth: each environment holds the one enclosing it, and a read further up walks
    that chain rather than copying — a copied cell would be a second cell.
    **per-iteration loop bindings** fall out of the same model: a captured loop
    target is a copy rather than a cell, and the environment holding one is
    allocated where the closure is written rather than once per frame

- **generators, `async` / `await`** — the state class is an environment plus a
    `$state` field, and a `yield` is a field write and a return. `yield from` and
    `await` are the same delegation, differing only in how the inner iterator is
    obtained. `throw` and `close` resume *by raising*, at the suspension, so a `yield`
    inside `try` reaches its own handler

- **the argument surface** — literal parameter defaults, keyword arguments in both
    directions, `*args` and `**kwargs`, keyword-only and positional-only parameters,
    `f(*args, **kwargs)` at a call site, and a decorated method (`@property` included)

- **displays and comprehensions** — `*` and `**` inside a display, built in runs so
    the unstarred elements still go in one op, and a comprehension with any number of
    `for` clauses

- **`with` blocks**, any number of items to a statement, and a cleanup stack so a
    `return` or a `break` runs the `finally` it is leaving

- **the assignment surface** — a tuple or list target (starred and nested included),
    chained assignment, augmented assignment to an attribute or an item, and the same
    target forms as a loop or comprehension variable. one `assign_to` serves all of
    them, and unpacking drives the *iterator* the way python does

- **exceptions, generally** — `raise <expr>` of any class or instance, `from`, a bare
    re-raise, and `except` against a user-defined class or a tuple of them. an
    `except` block marks its exception as *being handled*, so a raise inside it — or
    inside anything it calls — chains onto it the way python's does

- **escape hatches** — calling out of the unit, the iteration protocol, method
    calls, attribute access and global reads all go through the abstract object
    protocol, so an unspecialized construct still compiles at interpreter speed

- **passes** — copy propagation (twice), constant and branch folding, redundant-box
    and redundant-narrowing folding, unpacking a tuple built in the same block,
    reading a `frozen` field once across a call, dead registers, the infallible fixed
    point over the call graph, borrowing, and the release-set narrowing. the verifier
    checks the release sets independently of the pass that computes them

- **ergonomics** — `--annotate`, `--no-any`, `--require-native`, `by run --compiled`,
    `#line` back to the `.by`, declines as real diagnostics, and a rebuild cache
    keyed on the emitted C

M3 is largely covered by the protocol escape hatches (correct, not yet fast).

- **unboxed lists** — a `list` display of values that own nothing lives in a buffer
    of its own rather than as a `PyObject *` each, indexed at a compile-time width
    with the bounds check a `list` index does. the buffer carries **its own reference
    count**, so it retains and releases inside the ownership discipline rather than
    beside it, and the verifier rejects boxing one: a list that escapes has to have
    been a real list all along, because building one from the buffer is a *copy* and
    a copy is a different list

M4–M5 are open; the M6 items above are done. the coroutine family, which the open
questions below used to defer, is done too — so what is left of *coverage* is
inheritance and the specialized container operations that are currently
correct-but-interpreter-speed.

inheritance is done, and the decision it needed is worth stating. a **static**
`PyTypeObject` can be neither modified nor subclassed, and that is precisely what
licenses a **direct** method call: no override can exist, so there is nothing for a
vtable to dispatch on. a class that is decorated, extends another, or is extended by
another needs the opposite, so it is emitted as a **heap type** from a `PyType_Spec` —
and pays for it by dropping to the protocol, where an override is seen. an unrelated
class stays static and keeps the direct call. a subclass's struct begins with its
base's fields, which is python's own single-inheritance layout rule, so an upcast is
free and a downcast is the type check `By_UnboxInstance` already does.

a base whose **metaclass is not `type`** is not a spec's to build, and the abstract
base classes make that the common case rather than an exotic one. such a class is
built the way python builds one — by calling the metaclass with a namespace — which
also gets every dunder in the class body into its type slot for free, since
`type.__new__` runs the same fixup a class statement does. it costs the instance
layout, so it is open only to a class that adds no fields of its own; the split is
laid out in [the runtime model](runtime.md#how-a-type-is-built-at-import).

the differential harness is what made this pace possible, and it earned it: it caught
four **silent wrong answers** in already-shipped features — a `finally` skipped by an
early `return`, an unmatched exception in a nested `try` escaping its outer handler, a
shadowed `len` returning the builtin's answer, and a loop iterator lost across a
suspension. none of them would have shown up as a crash.

free-threading support is a lowering choice taken from M1 (the refcount ops are
abstract from the start) and a feature from M5 onward. the `parallel` block form
in [optimizations](optimizations.md#free-threading) is deliberately after
everything else

the [planned modifiers](planned-features.md) slot in where the pass they feed
already lives: `final T` with devirtualization and `StackAlloc` in M4–M5,
`literal T` with monomorphization in M5, `single T` with the tagged-union
representation in M4 — but only if it has shipped in ty by then, and it should
not gate the milestone if it has not

## risks

| risk                                                          | mitigation                                                                                                                                                                    |
| ------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| a ty inference bug becomes a segfault instead of a type error | the [representation invariant](ir.md#the-representation-invariant): every unproven narrowing carries a check. the checks are the existing soundness positions                 |
| two lowerings of one construct drift apart                    | the differential harness, on every mdtest block, on every commit                                                                                                              |
| C compile time dominates the edit loop                        | salsa-keyed per-function caching on optimized BIR, per-module TUs, parallel `cc`, and cranelift later                                                                         |
| the optimizer becomes untestable                              | `--annotate` and BIR snapshots make every decision inspectable; a pass that cannot be shown in `--annotate` is not finished                                                   |
| scope. this is the largest thing in the repo                  | the fallback hatches make every milestone shippable; nothing is blocked on the optimizer existing                                                                             |
| cpython API churn between versions                            | emitting C means we consume the headers, not a snapshot of them. the ABI test matrix covers each supported version                                                            |
| a second backend gets built before the first one is proven    | the `Backend` trait exists in the design; cranelift is explicitly not on the milestone list                                                                                   |
| the free-threading discipline is retrofitted                  | `IncRef` / `DecRef` are abstract from M0, which is cheap now and expensive later                                                                                              |
| a planned modifier lands with a shape codegen cannot use      | the [requests back to the language](planned-features.md) are written down before the features ship, not discovered after — `single` needs its witness *member*, not a boolean |
| a representation is silently pessimized rather than wrong     | `--annotate` must report boxing it could have avoided; the `SafeVariance` mapper rule is the motivating case                                                                  |

## open questions

- **nullable unboxed values.** `float | None` currently boxes. a nullable
    `double` needs a separate tag word, which is cheap in a struct field and
    awkward in a register. worth doing for `Optional[float]`-heavy numeric code?

- ~~**generators and `async`.**~~ **built.** the answer was the one the design
    predicted — the state class is a closure environment plus a `$state` field, and the
    dispatch is a chain of branches — and it needed **no new IR ops at all**: a `yield`
    is a field write and a return.

    the piece the design named as the blocker was right, and doing it first was right:
    a lowering mode where a local lives in a *field* rather than a register. it landed
    as `Place`, and it lifted the closure capture restriction on the way.

    what the design did *not* predict is the invariant that turned out to matter most:
    **no register may hold a value across a suspension.** a `yield` returns, so every
    register is gone when the machine is re-entered. a backward liveness check from each
    resumption point enforces it, and it caught a real bug — `for x in xs: yield x` kept
    the iterator in a register — before any test did.

    that check became a *transform*: the registers it names are parked in fields of
    their own, written before the `return` that suspends and read back at the
    resumption. a named local already had a field; a temporary did not, and python puts
    one across a suspension in the most ordinary code there is — `total + await step(i)`
    reads `total` first. the liveness that drives it runs over the flow a **suspended**
    frame has, with an edge from each suspension to its own resumption point, because
    the static shape says a suspension goes nowhere. a park slot takes the register's
    own representation rather than the `object` a cell is forced to, so the value
    survives the suspension unboxed.

- **`match` on non-sealed subjects.** the tagged-union path is clear; the general
    path needs a decision tree builder. reuse ty's exhaustiveness machinery, or
    build one?

- **cross-unit inlining.** tier 3 is per-unit. should a compiled dependency ship
    its BIR alongside the extension so a downstream unit can inline into it?
    that is a stable serialized IR format, which is a real commitment

- **how much does `sound-types` need to be on?** the biggest optimizations assume
    it. should `by compile` warn when it is off, or simply be much less effective
    and say so in `--annotate`?

- **`is` on unboxed values.** documented as unspecified above. should `buff` lint
    it, should the compiler refuse it, or should we box to preserve it?
