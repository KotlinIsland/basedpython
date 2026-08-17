# native compilation

> **status: implemented, and still growing.** `by compile` builds real
> extension modules today; the milestone list in [plan.md](plan.md) records what
> has shipped and what has not

`by compile` turns a set of `.by` or `.py` modules into a native CPython extension
(`.so` / `.pyd` / `.dylib`) that imports and behaves exactly like the
transpiled python it replaces:

```sh
by compile                 # compile the whole project to ./out/
by compile app.hot         # compile one module, leave the rest interpreted
python -c "import app"     # the extension is picked up ahead of the .py
```

the observable behaviour of a compiled module and its interpreted twin must be
identical. that is not an aspiration, it is the property the entire test
strategy is built on ([plan](plan.md#differential-testing))

## why a second backend and not a mypyc invocation

the obvious cheap move is to transpile `.by` to `.py` and hand the result to
mypyc. it does not work, and the reason is the whole thesis of this design

transpilation is **lossy by construction**. it exists to produce portable
python, so every basedpython-only fact is either erased or re-encoded in a form
the runtime tolerates:

| basedpython source         | transpiled python       | what the compiler needed |
| -------------------------- | ----------------------- | ------------------------ |
| `sealed class Shape`       | plain `class Shape`     | the closed subclass set  |
| `enum class` with payloads | a dataclass hierarchy   | a tagged union           |
| `def f(local buf: bytes)`  | `def f(buf: bytes)`     | the escape proof         |
| `def g() raises Never`     | `def g()`               | the infallible contract  |
| `x: float`                 | `x: JustFloat`          | an unboxed `double`      |
| `Array[Dim + 1]`           | `Array`                 | the shape arithmetic     |
| `s.character_count`        | `len(_by_graphemes(s))` | a native segmenter call  |

a compiler reading the transpiled output sees the second column. every fact in
the third column — every fact worth compiling for — has already been thrown
away. so the compiler must sit where the transpiler sits: directly on the `.by`
AST and ty's inferred types

## architecture at a glance

```text
                  .by or .py source
                         │
              ┌──────────┴──────────┐
              │  parse (ruff_python_parser)
              │  check (ty_python_semantic)
              └──────────┬──────────┘
                         │  AST + SemanticModel
              ┌──────────┴───────────────┐
              │  (.by only)              │
     by_transforms                    by_irbuild
   (source → python)              (source → BIR)
              │                          │
              │                     by_opt passes
              │                          │
              │                    by_codegen_c
              │                          │
              │                   cc + ld (platform)
              ▼                          ▼
          out/*.py                out/*.cpython-*.so
```

ordinary python enters the same front end. `by_irbuild` lowers the `.py` AST
directly, and nothing about that path routes through the transpiler — which is
what lets a `.py` file compile at all, and lets it be its own interpreted
fallback: it is already the thing that runs. the source language is one value,
`by_irbuild::Language`, and it settles three questions at once — how the source
parses, whether a loop's binding is fresh on each iteration (python shares one,
basedpython does not), and where a declined function's definition comes from

the two backends are **siblings**, not stages. they share a front end and
nothing else. this is the single most important structural decision in the
design, and it has one uncomfortable consequence: every surface construct now
has two lowerings that must agree. [plan](plan.md#differential-testing)
describes how that is held down

## goals

- **speed**, in that order of priority: hot numeric and string code first,
    attribute and method dispatch second, everything else third
- **total language coverage from day one**, via
    [interpreted fallbacks](plan.md#coverage-escape-hatches) — a construct with
    no native lowering still runs, just not fast
- **safety that does not depend on ty being right.** a checker bug must produce
    a `TypeError`, never a segfault. the unboxing rule in
    [ir](ir.md#the-representation-invariant) exists for this and nothing else
- **incremental compilation at function granularity**, falling out of salsa
    rather than bolted on
- **debuggability**: `.by` line numbers in gdb, in `perf`, and in tracebacks
- **no new toolchain for the user** — the c compiler cpython was built with

## non-goals

- compiling the standard library or third-party packages. calls out of the unit
    use the python calling convention and are guarded by the
    [soundness checks](../../features/soundness.md) that already exist
- a JIT. we compile ahead of time, and the ahead-of-time facts are the ones we
    have
- replacing cpython's object model, allocator, or GC
- beating a hand-written rust extension. beating cpython by 2–10× on typed code,
    and by considerably more where the type system lets us leave the object
    model behind entirely, is the target

## the tier ladder

compilation is not all-or-nothing. each tier buys more speed by assuming more,
and each assumption is one the user opts into explicitly:

| tier | name         | assumes                                            | unlocks                                                             |
| ---- | ------------ | -------------------------------------------------- | ------------------------------------------------------------------- |
| 0    | interpreted  | nothing                                            | today's `by build`                                                  |
| 1    | open world   | ty's types are right; runtime checks at boundaries | unboxing, native classes, direct field access                       |
| 2    | closed world | the compilation unit is not monkey-patched         | early binding, native calling conventions, devirtualization         |
| 3    | sealed unit  | `api.lock` is the complete public surface          | cross-module inlining, monomorphization, dead-code elimination, LTO |

tier 2 is the default for a project, tier 1 for a single module compiled out of
a larger interpreted program. tier 3 requires a current
[api lockfile](../../features/api-lock.md) — the lockfile stops being only a
review artifact and becomes the ABI contract. see
[optimizations](optimizations.md#the-lockfile-as-a-closed-world-boundary)

## what makes this different from mypyc

mypyc is the reference implementation of this idea and this design borrows its
structure without apology: typed IR, native classes, tagged integers, generated
C, a hand-written C runtime. the differences are all downstream of one thing —
**mypyc reads PEP 484 types, and we read basedpython's**

mypyc's own [future work list](https://github.com/python/mypy/blob/master/mypyc/doc/future.md)
asks for integer range analysis, and for a way to *enforce* that an attribute is
always defined — it already infers that by dataflow where it can
(`mypyc/analysis/attrdefined.py`), and what it is missing is the declaration. so
the difference is not that mypyc cannot work these facts out; it is that it has
nowhere to read them from:

| mypyc must infer or give up | basedpython declares it         |
| --------------------------- | ------------------------------- |
| integer ranges              | literal types, `Array[Dim + 1]` |
| always-defined attributes   | `data class`, `init` modifiers  |
| whether a value escapes     | `local`, `once`                 |
| whether a call can raise    | `raises Never`                  |
| the subclass set            | `sealed`                        |
| the public surface          | `api.lock`                      |
| a precise type for `x = 1`  | `analysis.sound-types`          |
| that `float` is not `int`   | by default                      |

the whole of [optimizations](optimizations.md) is that table, expanded

three more rows are in flight and not yet on `main`. they get their own document,
because two of them change this design rather than extend it:

| mypyc must infer or give up          | basedpython would declare it |
| ------------------------------------ | ---------------------------- |
| the exact runtime class at a place   | `final T`                    |
| a compile-time constant argument     | `literal T`                  |
| that a generic decomposes on a union | `single T`                   |

## doc map

- [technology](technology.md) — what we emit, what the runtime is written in,
    how it is packaged, and why not llvm / cranelift / rust / asm
- [ir](ir.md) — BIR: runtime types, ops, calling conventions, the pass
    pipeline, and incrementality
- [optimizations](optimizations.md) — every optimization the type system
    unlocks, ranked by payoff against cost
- [planned features](planned-features.md) — the five modifiers still in flight,
    what each buys the compiler, and what codegen needs their design to preserve
- [runtime](runtime.md) — object model, refcounting, exceptions, interop,
    debugging
- [benchmarks](benchmarks.md) — what the suite measures, the method it enforces,
    and what it refuses to time
- [plan](plan.md) — semantic deltas, testing, milestones, risks
