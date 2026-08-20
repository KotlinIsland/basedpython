# technology choices

"what should we compile to" is really four independent questions, and they have
four different answers:

1. what does the backend **emit**? → **C11**
1. what is the **runtime library** written in? → **C headers, plus rust for
    self-contained algorithms**
1. what **ABI** do we target? → **the full cpython API**, not the limited one
1. how is it **packaged**? → **our own PEP 517 backend**, not maturin

each is argued below

## 1. the emission target

### the criterion that settles it

almost everything that makes compiled python fast is a **semantic** decision:
which values can skip the object header, which calls can skip the namespace
lookup, which refcount pairs cancel, which `isinstance` chain is a jump table,
which generic instantiation gets its own body. every one of those happens in
**our** IR, driven by **our** type system, before the backend ever sees the
program

what is left for the backend is the classical part — instruction selection,
register allocation, scheduling, peepholes, inlining of small leaf functions.
that work is a solved commodity, and the question is only which commodity is
cheapest to buy

### the options

| target          | peak speed | build-time cost to us | user toolchain       | cpython API access     | verdict        |
| --------------- | ---------- | --------------------- | -------------------- | ---------------------- | -------------- |
| **C11**         | best       | none                  | the one cpython used | native — it is headers | **chosen**     |
| llvm ir         | best       | very high             | none extra           | must be re-declared    | rejected       |
| cranelift       | fair       | moderate              | none                 | must be re-declared    | deferred       |
| rust            | best       | high                  | rustc                | via `pyo3-ffi`         | rejected       |
| asm             | best       | absurd                | none                 | must be re-declared    | rejected       |
| python bytecode | none       | low                   | none                 | native                 | not a compiler |

### C11 — chosen

- **the cpython API is a C header API.** `Py_INCREF`, `PyTuple_GET_ITEM`,
    `PyList_SET_ITEM`, the `PyObject` layout, the type-object slots — these are
    macros and `static inline` functions in `Python.h`. emitting C means we get
    them for free, at whatever version of cpython the user is building against,
    including the ones that changed shape between 3.12 and 3.14. every non-C
    target has to re-declare that surface and re-declare it *per cpython
    version*. this is not a small tax, it is the dominant one
- **the compiler is already installed.** anyone who can build a C extension can
    build ours, on every platform cpython runs on, including the ones we would
    never get llvm or cranelift onto
- **it costs us nothing to ship.** no vendored backend, no version pin, no
    100MB dependency, no impact on `by`'s own build time
- **the optimizer is excellent and free.** `-O2` plus thin LTO across the unit
    gives us inlining, scalar replacement, and vectorization we will never write
- **it is debuggable.** `#line` directives point gdb, lldb, `perf`, and
    sanitizers at `.by` source (we already have the
    [sourcemaps](../sourcemaps.md) to generate them), and `by compile --annotate`
    can show the generated C next to the source that produced it
- it is proven: mypyc has taken this exact route for years, with ~28k lines of C
    runtime behind it

the costs are real and we accept them: C is a poor IR (no `goto`-into-scope, no
guaranteed tail calls, no control over stack slots), MSVC needs its own dialect
handling, and a compile of a large unit is dominated by the C compiler rather
than by us. the mitigations are per-function translation units, aggressive
caching, and the deferred cranelift backend below

### llvm — rejected

the case for llvm is cross-compilation and skipping the C front end's parse
time. against it:

- **it does not buy us optimizations we lack.** we would be handing llvm the
    same IR clang would produce from our C, minus the header knowledge
- **it costs a version pin and ~100MB.** building `by` would require a matching
    llvm; `inkwell` tracks llvm releases, and llvm's IR is not stable across
    them. this is the single largest possible increase in our build complexity
- **the refcounting idiom does not optimize itself.** llvm will not elide
    `Py_INCREF`/`Py_DECREF` pairs across an opaque call without help, and the
    help is a custom pass — which is work we would do in our own IR anyway,
    where it is easier
- we would still need a linker and a platform C toolchain to produce a loadable
    extension, so the "no toolchain" win is partly illusory

### cranelift: deferred, not rejected

cranelift is a pure-rust codegen backend with excellent compile speed and
mediocre output quality. that profile is wrong for release builds and *exactly
right* for a dev loop:

```sh
by compile --backend=cranelift    # seconds, not minutes; no cc needed
by compile --backend=c            # the release path
```

it is deferred because the cpython API re-declaration problem applies in full,
and because we should not build two backends before we have proven one. the
design accommodates it by keeping codegen behind a trait
([ir](ir.md#the-backend-boundary)) — BIR after lowering is deliberately close to
a three-address machine IR, so a cranelift backend is a new consumer of an
existing interface rather than a rewrite

### rust — rejected as an emission target

superficially attractive: the workspace is already rust, `pyo3` exists, and the
generated code would be memory-safe. it does not survive contact:

- **generated code wants `goto`.** lowered IR has irreducible control flow from
    exception edges and loop breaks. rust has no `goto`, and encoding a CFG as a
    `loop { match state { … } }` state machine defeats rustc's own optimizer
- **borrowck fights us.** we are emitting manual refcounting over raw
    `*mut PyObject`. every line would be `unsafe`, which throws away the only
    thing rust was offering
- **rustc is slower than clang** on machine-generated code, and it is the part of
    the build we cannot cache away
- **`pyo3` is the wrong abstraction level.** it is designed to make hand-written
    rust ergonomic; we want the raw slots

rust *is* the right language for large parts of the compiler and for parts of
the runtime — just not for the code we generate

### asm — rejected

we would be writing a register allocator, a scheduler, an object-file emitter,
and unwind-table generation, per architecture, to reach parity with `-O2`. there
is no scenario in which this is the constraint

## 2. the runtime library

generated C is thin; the substance lives in a runtime library (mypyc's `lib-rt`,
ours `by_rt`). two languages, split on one criterion — **does it need to inline
into generated code?**

### C headers for anything hot

`inc_ref`, `dec_ref`, tagged-integer arithmetic, list/dict/str fast paths,
tuple unboxing, error-value checks, the sealed-tag test. these are single-digit
instruction sequences whose entire value is that they inline. they live in
`by_rt/include/by.h` as `static inline`, exactly as mypyc's `CPy.h` does

a rust `staticlib` cannot inline into a C translation unit without cross-language
LTO, which requires matching clang and rustc llvm versions and fails in ways that
are miserable to diagnose. we will not stake the hot path on it

### rust for self-contained algorithms

anything with a real algorithm behind it, no cpython API contact, and a call
boundary that is cheap relative to the work:

| runtime piece                        | language | why                                                |
| ------------------------------------ | -------- | -------------------------------------------------- |
| refcount, boxing, tagged int math    | C        | must inline                                        |
| list / dict / str / tuple fast paths | C        | must inline, and they *are* cpython macros         |
| exception machinery                  | C        | touches thread state and frame objects             |
| grapheme segmentation                | rust     | a real unicode algorithm we already depend on      |
| compiled regex engines               | rust     | see [optimizations](optimizations.md#regex-shapes) |
| decimal / float formatting           | rust     | correctness-critical, self-contained               |
| sort comparators for primitive keys  | rust     | pattern-defeating quicksort, no API contact        |

this is a genuine win and not just tidiness. `s.character_count` today lowers to
`len(_by_graphemes(s))` — a python-level polyfill. compiled, it becomes a call
into the same segmenter crate `buff` already links, over the string's UTF-8
bytes, with no intermediate list

the rust half ships as a prebuilt `staticlib` inside the `by` wheel, one per
platform. this is not new distribution work — `by` is already a per-platform
wheel, so the runtime archive rides along and the user never needs rustc

## 3. the ABI

we target the **full cpython API** — not the limited API (PEP 384), and not the
stable ABI

the limited API would let one wheel serve every cpython 3.x, which is a real
distribution win. the price is every fast path we are building the compiler for:

| what we need                           | limited API                           |
| -------------------------------------- | ------------------------------------- |
| `PyObject` / `PyVarObject` layout      | opaque                                |
| `PyTuple_GET_ITEM` / `PyList_SET_ITEM` | unavailable                           |
| static type objects with custom layout | heap types only, indirect slot access |
| direct `ob_refcnt` manipulation        | function call                         |
| `PyUnicode` internal representation    | opaque                                |

paying a function call for every field read is the opposite of the exercise.
mypyc reached the same conclusion. this is worth revisiting only if
[HPy](https://hpyproject.org) stabilizes with a performant CPython ABI mode

### an artefact is pinned to one minor version, and says so itself

taking the full API means the runtime header reads layouts that move between
versions, so `by.h` is full of `#if PY_VERSION_HEX` branches — and those are
decided by the headers the *build* compiled against. an artefact loaded by a
different minor version therefore runs branches written for a layout that
interpreter does not have, which is a crash rather than a wrong answer

cpython does not prevent this. the version tag lives in the **file name**, and
every 3.x also lists a bare `.so` in `EXTENSION_SUFFIXES` — so an artefact that
is renamed, or copied out of a wheel built elsewhere, is offered to whatever is
running. that is not hypothetical: `argparse` built for 3.13 and renamed
segfaults inside a type construction under 3.14, in a build with no marshalled
code object in it at all

so every emitted module refuses one itself. `PyInit_` calls
`By_InterpreterMatches` before it hands its module definition over — before
anything of the build's own layout is read — and a mismatch is an `ImportError`
naming both versions. the reading is `Py_GetVersion` rather than the newer
`Py_Version`, because it is the one every version this header compiles against
exports: a module built against newer headers naming a symbol the running
interpreter lacks would be the same failure by another road

this is the general form of the check the marshalled fallback makes for itself.
that one compares the bytecode magic, which moves for a different reason and can
move within a micro release, and a disagreement there *declines* to the embedded
source rather than refusing the import — the code object is a cache, while the
compiled code is the module

### free-threading is a design constraint now, not a migration later

cpython 3.13 introduced free-threaded builds and 3.14 made them supported.
retrofitting a compiler onto a different refcounting discipline is expensive, so
BIR takes the constraint up front: **`IncRef` / `DecRef` / `Borrow` are abstract
ops**, and the lowering picks the discipline:

| build         | `IncRef` lowers to                                        |
| ------------- | --------------------------------------------------------- |
| GIL           | `++ob_refcnt`, elided entirely for immortals              |
| free-threaded | biased refcounting on the owning thread, atomic otherwise |

before any of that, there is a much blunter obligation. on a free-threaded 3.13+
build, importing an extension that does **not** declare

```c
{Py_mod_gil, Py_MOD_GIL_NOT_USED}
```

makes the interpreter re-enable the GIL for the whole process. an extension we
compiled for speed would therefore *serialize the user's entire program* the
moment it was imported — the single most expensive thing a compiled module could
silently do. so every emitted module declares the slot, guarded on
`PY_VERSION_HEX >= 0x030D0000` because it does not exist earlier

the declaration is honest rather than optimistic: compiled functions hold no
shared mutable state — every register is a frame local, and refcounting goes
through cpython's own macros, which are correct under either discipline

mypyc declares the same slot, so this is table stakes rather than a
differentiator. the differentiator is the row above it — what `local` and
`frozen` let the *lowering* do once they are read

the interesting part is that basedpython has something to *say* here beyond
surviving. `frozen data class` is deeply immutable and `local` proves
non-escape — together they are a static proof that a value is unshared or
unmutated, which is precisely what a free-threaded runtime cannot otherwise
know. see [optimizations](optimizations.md#free-threading)

## 4. packaging

**not maturin.** maturin builds python extensions *from rust crates* — it wraps
cargo. our input is `.by` source and our output is C; there is no cargo project
to wrap. using it would mean generating a synthetic crate per module, which is
all of maturin's constraints and none of its benefits

instead, three entry points over one core:

### the CLI

```sh
by compile                          # whole project → out/
by compile app.hot app.parse        # a subset; the rest stays interpreted
by compile --tier=1                 # open world (see index.md)
by compile --annotate               # emit C next to the .by that produced it
by compile --backend=cranelift      # when it exists
```

`by run` gains `--compiled`, which compiles the reachable modules and then
imports the extension — the same ergonomics as today, so the fast path is one
flag away from the normal loop

### a PEP 517 build backend

```toml
[build-system]
requires = ["basedpython"]
build-backend = "basedpython.build"
```

`pip install .` then compiles the project and produces a platform wheel. this is
the path that matters for shipping a basedpython library to pypi

### a setuptools hook

for projects that already have a `setup.py`, mirroring `mypycify`:

```python
from basedpython.build import bycompile

setup(ext_modules=bycompile(["src/app/hot.by"]))
```

### the output directory is importable on its own

the extension embeds its own transpiled python as the interpreted fallback, so it
needs whatever that python needs at import time. that turns out to be nothing
extra, but only because the transpiler is configured the same way `by build`
configures it: the [lazy-import pass](../../features/lazy-imports.md) binds
`JustFloat = float` locally rather than emitting a `from ty_extensions import …`
that has no module behind it

this is a sharp edge worth naming. transpiling for the fallback with a
*different* config than the interpreted build uses produces an extension that
fails to import, and it fails at module init — so every function in it is gone,
not just the declined ones. the driver uses `Config::default()` for exactly this
reason, and the differential harness transpiles its interpreted leg the same way
so the two legs stay the same program

### the entry-point problem, which we do not have

mypyc's most-cited wart is that `if __name__ == "__main__":` cannot work in a
compiled module, so `python -m mod` breaks. basedpython's entry point is already
[a `main` function](../../features/main-function.md), not a `__name__` guard, so
`by compile` emits a console-script shim that imports the extension and calls
`main()` with the parsed arguments. the wart is designed out rather than
documented around

## what we are explicitly not building

a linker, a garbage collector, a JIT, an object-file writer, an unwinder, a new
object model, or a second type checker. every one of those has a cpython or
platform implementation we should be calling instead
