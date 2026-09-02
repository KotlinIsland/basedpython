# BIR — the basedpython intermediate representation

BIR is a typed, block-structured, mostly-SSA IR. it is built from the `.by` AST
plus ty's `SemanticModel`, optimized in place by a fixed pass pipeline, lowered
until every remaining op has a direct C translation, and emitted

## crates

| crate          | contains                                                      |
| -------------- | ------------------------------------------------------------- |
| `by_ir`        | `RType`, `Value`, `Op`, `Function`, the printer, the verifier |
| `by_irbuild`   | AST + `SemanticModel` → BIR                                   |
| `by_opt`       | the pass pipeline                                             |
| `by_codegen_c` | BIR → C11                                                     |
| `by_rt`        | the runtime headers and the rust `staticlib`                  |
| `by_build`     | driver, cc invocation, caching, wheel and extension assembly  |

`by_ir` depends on nothing but `ruff_text_size` and `ruff_index`. `by_irbuild`
is the only crate that sees `ty_python_semantic`. this keeps the optimizer
honest: **once a fact is not in BIR, it does not exist**, so anything the
optimizer needs must be recorded at build time as an explicit annotation rather
than re-derived by reaching back into the checker

## runtime types

an `RType` is a *representation*, not a static type. `list[int]`, `list[str]`,
and `list[T]` all erase to one `RType`. the erasure is the point: an `RType`
answers "what do the bits look like", and nothing else

### the representation invariant

> compiled code may assume, without checking, that every register holds a value
> matching its `RType`

this is what makes the generated code fast and it is also the only thing
standing between a checker bug and a segfault. so it comes with a hard rule:

> narrowing a value to a more precise `RType` requires either a **proof** or an
> inserted **check**. never a hope

and a pleasant consequence — the positions that need checks are exactly the
positions [runtime soundness checks](../../features/soundness.md) already
enumerates:

| soundness position | what it becomes when compiled              |
| ------------------ | ------------------------------------------ |
| `generic-calls`    | an unbox check on a typevar-derived result |
| `projections`      | an unbox check on a container read         |
| `iterations`       | an unbox check per loop element            |
| `assignments`      | an unbox check at a gradual → typed edge   |
| `returns`          | a check before returning to typed code     |
| `arguments`        | a check on a call into non-compiled code   |
| `parameters`       | the python-wrapper entry check             |

the compiler does not invent a check discipline; it reuses one that is already
specified, already tested, and already toggleable. `by compile --soundness none`
is therefore a *trust-me* flag, and is documented as one: it removes the
guardrail between a wrong annotation and undefined behaviour

the corollary is worth stating, because it is what makes gradual code compilable
at all: **`object` needs no check**. it is the widest representation, so nothing
is being assumed and nothing has to be proven. a gradual value therefore lowers
to an `object` register with no guard, and every operation on it goes through the
abstract object protocol — the same work the interpreter would have done

### primitives

| `RType`                                        | C repr                          | boxed     | notes                                                     |
| ---------------------------------------------- | ------------------------------- | --------- | --------------------------------------------------------- |
| `object`                                       | `PyObject *`                    | yes       | the top of the lattice                                    |
| `int`                                          | `CPyTagged`                     | no        | 1-bit tag: even = value `>> 1`, odd = pointer             |
| `i8`…`i64`, `u8`…`u64`                         | `int8_t`…`uint64_t`             | no        | only where a range proves it fits                         |
| `f64`                                          | `double`                        | no        | see [no number promotions](optimizations.md#exact-floats) |
| `bool`                                         | `char`                          | no        |                                                           |
| `bit`                                          | `char`                          | no        | a comparison result that cannot be an error               |
| `None`                                         | nothing                         | no        | a zero-width type; only the tag matters                   |
| `str`, `bytes`, `list`, `dict`, `set`, `tuple` | `PyObject *`                    | yes       | with known layout, so element access is a field read      |
| `RTuple`                                       | a C struct                      | no        | fixed-length tuples are unboxed                           |
| `RInstance`                                    | `<Native> *`                    | yes       | a native class, see [runtime](runtime.md#native-classes)  |
| `RUnion`                                       | a tagged struct or `PyObject *` | sometimes | see below                                                 |
| `RCharacter`                                   | `PyObject *` (a `str` subclass) | yes       | may unbox to `u32` + a cluster length                     |

`int` is arbitrary-precision, as in python. the tag is a small-value fast path,
not a range restriction

### the basedpython additions

three `RType`s exist because basedpython's type system produces facts mypyc's
does not:

**`RUnion` driven by `sealed`.** a union over a
[sealed hierarchy](../../features/sealed-classes.md) has a statically complete
member list, so it becomes a tagged union — a `u8` discriminant plus the widest
payload — rather than a `PyObject *` with `isinstance` chains. an
[enum class](../../features/enums.md) is the same thing with nicer syntax:

```by
enum class Shape:
    case Circle(radius: float)
    case Rect(width: float, height: float)
```

```c
typedef struct { uint8_t tag; union {
    struct { double radius; } circle;
    struct { double width, height; } rect;
} p; } Shape;
```

a `Shape` that never escapes to interpreted code never allocates at all

**`RRange`-annotated integers.** an `i32` carries the interval that justified
it. the interval comes from a declaration
([symbolic type operations](../../features/symbolic-type-ops.md)) or from the range pass, and it
survives into codegen so that overflow checks and bounds checks can be dropped
individually rather than all-or-nothing

**`RBorrowed`.** a modifier on any boxed `RType` meaning *this register does not
own a reference*. mypyc infers borrows within a function; here it is also a
**declared, interprocedural** fact, because
[`local`](../../features/local-lifetimes.md) says so in the signature

**an `exact` bit on `RInstance`**, meaning the value's runtime class is exactly
this class and not a subclass. it comes either from a `@final` class or from a
use-site [`final T`](planned-features.md#final-t-exactness-at-the-use-site),
and it is what licenses direct dispatch and a pinned instance size

### mapping ty types to RTypes

`by_irbuild::mapper` implements `Type → RType`. the interesting cases are the
ones where basedpython's type is *narrower than python's would be*:

| ty type                           | `RType`                        | why                                        |
| --------------------------------- | ------------------------------ | ------------------------------------------ |
| `int`                             | `int` (tagged)                 |                                            |
| a union of `Literal` ints         | `u8`                           | the range fits                             |
| `float`                           | `f64`                          | `float` excludes `int` in `.by`            |
| `float \| None`                   | `object`                       | a nullable double needs a box, for now     |
| `tuple[int, str]`                 | `RTuple`                       | fixed length                               |
| a `@final` class, or `final C`    | `RInstance { exact }`          | no subclass can override                   |
| a `sealed` union                  | `RUnion` tagged                | the member set is closed                   |
| `A[X \| Y]` where `A` is `single` | `RUnion` tagged, tag = witness | the type argument has one runtime witness  |
| `LiteralValue(v)`                 | a static constant              | folds; keys a value-specialization         |
| `LiteralString`                   | `str`                          | provenance only — not a compile-time value |
| `SafeVariance[T]` in a parameter  | the **call-site** type         | the erasure has no representation content  |
| `A & not None`                    | `RInstance`, non-null          | narrowing removed the null check           |
| `Any` / `Unknown`                 | `object`                       | and a check wherever it is narrowed        |
| `T` (unspecialized)               | `object`                       | unless monomorphized                       |

the last four rows come from modifiers that have not landed yet;
[planned features](planned-features.md) works through what each one buys, what it
costs, and the three ways the mapper can get them subtly wrong

## values and ops

a `Function` is a list of `BasicBlock`s over `Value`s. `Value` is a `Register`,
a constant (`Integer`, `Float`, `CString`), or an `Op` result. blocks end in a
`ControlOp`

the op set follows mypyc's closely — that design is well-tested and there is no
value in being different for its own sake. grouped, as designed:

| group      | ops                                                               |
| ---------- | ----------------------------------------------------------------- |
| control    | `Goto`, `Branch`, `Return`, `Unreachable`, `Switch`               |
| data       | `Assign`, `LoadLiteral`, `LoadGlobal`, `LoadStatic`, `InitStatic` |
| calls      | `Call`, `MethodCall`, `CallC`, `PrimitiveOp`, `CallInterpreted`   |
| objects    | `GetAttr`, `SetAttr`, `New`, `TupleGet`, `TupleSet`               |
| conversion | `Box`, `Unbox`, `Cast`, `Truncate`, `Extend`                      |
| arithmetic | `IntOp`, `FloatOp`, `ComparisonOp`, `FloatComparisonOp`           |
| memory     | `LoadMem`, `SetMem`, `GetElement`, `SetElement`, `LoadAddress`    |
| refcount   | `IncRef`, `DecRef`, `KeepAlive`, `Borrow`, `EndBorrow`            |
| errors     | `LoadErrorValue`, `RaiseStandardError`                            |

### what the op set actually looks like

the implementation is narrower than the design above and differs in two ways worth
recording, because both were decisions rather than omissions:

- **no `Switch`, and none needed yet.** a chain of `Branch`es is a jump table, and
    the C compiler builds the table. that held up under the one thing that most wanted
    a switch — a generator's resumption dispatch. `Switch` earns its place when a tagged
    union needs a dense dispatch, not before
- **no `Yield`, either.** a suspension is a field write and a `Return`; the whole
    generator and coroutine surface is built from ops that already existed. that is the
    strongest evidence the closure-environment design was the right shape
- **an op has exactly one destination.** `DelegateStep` needs two answers — a value and
    whether the inner iterator finished — and returns a fixed `(object, bit)` *tuple*
    rather than writing two registers. a second destination would be invisible to
    `Op::dest`, and a register liveness never kills is a register nothing ever releases
- **no op for a chained environment read.** a name several frames up is a `GetField`
    of `$outer` per link and then an ordinary field read, so nesting depth is a
    *frontend* concern and the op set does not grow with it
- **refcounting is not in the op set.** `IncRef`/`DecRef`/`Borrow` are codegen's
    business, driven by `RegisterDecl.borrowed` and `BasicBlock.owned_at_exit`.
    keeping them out of the IR is what lets the verifier check the *discipline*
    rather than check each op against a hand-written expectation

the ops that exist, beyond the arithmetic and control-flow ones above:

| op                                                | does                                                         |
| ------------------------------------------------- | ------------------------------------------------------------ |
| `GetField` / `SetField`                           | a struct access at a compile-time offset. infallible         |
| `GetCell`                                         | a *shared* closure cell — starts unset, so it can fail       |
| `Enter` / `ExitContext`                           | the context-manager protocol                                 |
| `DelegateIter` / `DelegateStep`                   | `yield from` and `await`, which are one mechanism            |
| `RaiseWith`                                       | a raise carrying a value — `StopIteration(v)`                |
| `RaiseObject`                                     | `raise <expr>`, optionally `from <cause>`                    |
| `FinishFrame`                                     | a resumable frame ran to its end — deliberately not a raise  |
| `Unpack`                                          | a target list, landing in a fixed-length tuple               |
| `Extend` / `ToTuple`                              | a `*` or `**` in a display, and a starred tuple display      |
| `CallUnpacked`                                    | `f(*args, **kwargs)`, bound at runtime                       |
| `PushHandled` / `PopHandled`                      | enter and leave an `except` block, so a raise inside chains  |
| `CallNative`                                      | a call to a function or method in this unit, owner-qualified |
| `CallValue`                                       | a call to a callable held in a register                      |
| `CallPython` / `CallMethod`                       | a call resolved by name, or on a receiver                    |
| `LoadGlobal`                                      | the module namespace then its builtins, resolved per read    |
| `ImportModule` / `ImportFrom`                     | `from x import y` — the module with a fromlist, then a name  |
| `NewInstance` / `MakeClosure`                     | allocate an emitted class; bind a method to it               |
| `GetAttr` / `SetAttr`                             | the attribute protocol, for a receiver with no layout        |
| `GetIter` / `IterNext`                            | the iteration protocol                                       |
| `BuildList` / `Set` / `Tuple` / `Dict`            | a display                                                    |
| `GetItem` / `SetItem` / `Len`                     | subscripting and length                                      |
| `StrConcat` / `StrCompare` / `StrGetItem`         | `str` operations the operand type settles without dispatch   |
| `Format`                                          | one f-string field                                           |
| `FetchException` / `ExceptionMatches` / `Reraise` | the `except` machinery                                       |
| `Box` / `Unbox` / `Truthy` / `IsNull`             | representation changes and tests                             |

the ops that exist because of basedpython:

| op                       | comes from                                             | does                                                           |
| ------------------------ | ------------------------------------------------------ | -------------------------------------------------------------- |
| `TagOf` / `SealedSwitch` | `sealed`, `enum class`                                 | read a discriminant; branch on it as a jump table              |
| `CallInfallible`         | `raises Never`                                         | a call with **no error check emitted after it**                |
| `StackAlloc`             | `local`, escape analysis                               | allocate an instance in the caller's frame                     |
| `Borrow` / `EndBorrow`   | `local` parameters                                     | mark a register as non-owning across a call boundary           |
| `ReifiedArg`             | [reified generics](../../features/reified-generics.md) | pass a type as a runtime value                                 |
| `AssertRange`            | literal types, the range pass                          | a checked narrowing that later ops may assume                  |
| `Intrinsic`              | extensions, `Character`, regex                         | a direct call into `by_rt` in place of a python-level polyfill |

`CallInfallible` deserves emphasis: in mypyc-style output, roughly one branch
per call exists solely to propagate errors. removing it where `raises Never`
applies is not a micro-optimization, it is a structural reduction in the size
and branchiness of the generated code

## calling conventions

a callee may have up to three entry points. which ones exist is decided by the
tier and by `api.lock`:

| convention          | args                               | errors                             | who calls it                           |
| ------------------- | ---------------------------------- | ---------------------------------- | -------------------------------------- |
| `native`            | unboxed, positional, no kwargs     | sentinel return + `PyErr_Occurred` | compiled code in the same unit         |
| `native-infallible` | unboxed, positional                | **none** — cannot fail             | compiled code, when `raises Never`     |
| `python`            | `PyObject *` args, kwargs, `*args` | `NULL` return                      | interpreted code, and cross-unit calls |

the wrapper generation rule:

```text
in api.lock              → emit `python` (the ABI is public and must stay stable)
                           + `native` for in-unit callers
final or not in api.lock → emit `native` only, at tier 3
raises Never             → the native entry is `native-infallible`
```

an argument passed as `local` is passed **borrowed** — the callee does not
`IncRef` on entry and does not `DecRef` on exit. this is a calling-convention
change justified by a checked declaration, and it is not something mypyc can do,
because it has no way to know the callee will not keep the value

## the pass pipeline

ordered. passes marked ★ exist only because of basedpython's type system

```text
build
  01  irbuild                    AST + SemanticModel → BIR
  02  verify

analyse
  03 ★ exception-set             attach the `raises` set to every call site
  04 ★ escape                    interprocedural, seeded by `local` / `once`
  05 ★ range                     interval analysis, seeded by literal types
  06    reachability             dead block elimination

optimize
  07 ★ devirtualize              final / sealed / closed-world → direct calls
  08 ★ monomorphize              reified and inferred specializations
  09 ★ inline                    once-callbacks, trailing lambdas, small leaves
  10    constant-fold            reusing ty's literal folds where possible
  11    unbox                    representation selection, guided by 05
  12 ★ error-path-elide          drop error checks after infallible calls
  13    cse + licm
  14 ★ stack-promote             non-escaping allocations → frame slots
  15    verify

lower
  16    uninit-check             insert checks for possibly-undefined locals
  17    exception-edges          make error propagation explicit in the CFG
  18    refcount                 insert IncRef / DecRef, honouring borrows
  19    lower-primitives         PrimitiveOp → CallC / IntOp / LoadMem
  20    verify

emit
  21    codegen-c
```

`by_opt` implements this shape today for three of them — copy propagation, dead
register elimination, and infallibility inference — as `Fn(&mut ModuleIr)` with
the verifier run after each in debug builds. infallibility is derived
structurally for now; the `raises` clause is the eventual source, and it is
strictly stronger (integer arithmetic stays fallible under the structural rule
because the boxed path allocates, and only range analysis can rule that out)

the verifier runs after every phase in debug builds and checks: type consistency at every op, every block
terminated, no use before definition, refcount balance on every path, and
`RBorrowed` never stored into a heap location

## the backend boundary

after pass 20, BIR is a three-address machine IR with explicit refcounting,
explicit control flow, and no remaining python-level operations. `by_codegen_c`
consumes exactly that, through a trait:

```rust
trait Backend {
    fn emit_function(&mut self, f: &Function, ctx: &EmitContext) -> Result<()>;
    fn emit_module_init(&mut self, m: &ModuleIr) -> Result<()>;
    fn finish(self) -> Result<Vec<Artifact>>;
}
```

a cranelift backend is a second implementor. this is the *only* reason lowering
is a separate phase from emission — otherwise it would be cheaper to emit C
directly from optimized BIR

## incremental compilation

mypyc's slowest-to-live complaint is build time. we have a structural answer
that it does not: **ty is already incremental, so make BIR a salsa query**

```rust
#[salsa::tracked]
fn bir_function(db: &dyn Db, definition: Definition) -> Function;

#[salsa::tracked]
fn optimized_bir_function(db: &dyn Db, definition: Definition) -> Function;
```

- editing a function body invalidates that function's BIR and nothing else
- editing a *signature* invalidates callers, and only the callers — which is
    already how ty's dependency tracking behaves
- a comment or formatting change invalidates nothing at all
- the C file emitted for a function is cached under a hash of its **optimized
    BIR**, not its source. two different spellings of the same program share a
    cache entry, and a reformat is free

the C compiler is the remaining serial cost. mitigations, in order of value:

1. **one translation unit per module**, with per-function `#include`d fragments,
    so a single changed function rebuilds one TU
1. **parallel `cc` invocations**, one per TU
1. **thin LTO** at link time, so per-TU splitting does not cost cross-function
    inlining
1. the deferred [cranelift backend](technology.md#cranelift-deferred-not-rejected)
    for the dev loop

per the workspace's salsa conventions: BIR values are cached, so collections
inside `Function` are boxed slices, and anything built by `extend` or `collect`
is shrunk before it is returned
