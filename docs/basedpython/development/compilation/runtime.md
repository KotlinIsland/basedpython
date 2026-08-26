# the runtime model

what a compiled module looks like once it is loaded, and how it coexists with
interpreted code

## the object model

three kinds of class can appear in a compiled module:

| kind       | when                                                 | representation                                |
| ---------- | ---------------------------------------------------- | --------------------------------------------- |
| **native** | the default                                          | a C extension type, fixed layout              |
| **tagged** | a `sealed` / `enum class` hierarchy that stays local | a discriminant plus a payload union           |
| **boxed**  | general multiple inheritance, dynamic bases          | an ordinary python class built at module init |

### native classes

a native class is a static `PyTypeObject` with a C struct instance layout:

```c
typedef struct {
    PyObject_HEAD
    CPyTagged x;          // int
    double    y;          // float — unboxed, see optimizations.md
    PyObject *name;       // str
} PointObject;
```

attribute access is a field read at a compile-time offset, not two dict lookups.
this is the same trade `__slots__` makes, and basedpython already emits
`slots=True` for [`data class`](../../features/modifiers.md), so for the most
common class form the semantics are unchanged

#### what the layout is made of

the fields are the attributes the class body is seen to give the receiver:

- one every path through `__init__` assigns is a plain field
- one only some paths assign, or that only a later method assigns, takes a
    presence byte beside it, so a read that comes too early answers
    `AttributeError` the way python does
- a name `__slots__` declares that nothing assigns is that same optional field

the shape of the *statement* does not come into it. `self.a, self.b = pair()`,
`for self.item in xs`, `with open(p) as self.file`, `self.a = self.b = v` and
`self.total += 1` each give the instance an attribute exactly as `self.a = v`
does, and every one of them is a field. a write the pass could not read declines
rather than falling back to `PyObject_SetAttr`: a field write is an offset and a
dict write is not, and the compiler is not entitled to pick the second because it
failed to read the first.

two names can never be fields, because what they stand for is not storage of the
instance's own: `__dict__` is the instance namespace and `__weakref__` is support
a type spec does not add. a class that reads or writes either is left to its
interpreted definition, and so is one whose attribute name is only known at
runtime, which is what a `setattr` on the receiver is.

#### the dict beside the layout

the layout is not the whole instance. python lets a program give an object a name
its class never mentioned — `o.brand_new = 7` — and an instance that was only its
layout raised there, in the middle of a working program, where the interpreted
twin stored the value. so an emitted class keeps an instance dict beside its
layout, and the two divide the work: a declared field is read at its offset and
never goes near the dict, and the dict holds only what the layout has no room
for.

`__slots__` decides which classes take one, because it is python's own way of
saying an instance's attributes are exactly the declared ones — a class that
writes it is asking for precisely the bare layout, and giving it a dict anyway
would be the *opposite* divergence: accepting what the interpreted twin refuses.
python asks the whole chain rather than one class, so a `__slots__` over a base
that declares none still has the dict that base gave the instance.

the dict is a **managed** one, which python keeps in the pre-header — so the
struct, its base's prefix and every offset a compiled function reads are
untouched. what it costs is allocation: the pre-header and the two words a
collected type carries grow every instance, and a dict of arbitrary values has to
be one the collector can reach. that is four extra words, which for a two-field
class is a doubling — `alloc` went 7.38x → 5.62x against cpython and `objects`
17.23x → 12.48x, both about a quarter. `fields`, `methods`, `inherit`, `dot` and
`generic` do not move at all: a declared field is read and written at the offset
it always was, and the interpreted side still reaches it through a data
descriptor, which wins over the dict.

it is only allocation, so `__slots__` gets all of it back — the same source built
with the declaration times identically to the same source built before the dict
existed. the cost is not the tracking either: untracking every instance the
moment its constructor returns moved neither benchmark, which is what says the
four words are the whole of it.

nothing below python 3.13 publishes a way to walk or release a managed dict, so
there the class is built exactly as it was before dicts existed and refuses the
new attribute again. a class whose *generated* code cannot run without a dict
is not left to that. `@dataclass` writes an `__init__` that assigns one attribute
per annotation, and that is ordinary python assuming an ordinary instance, so on
a bare layout every one of those assignments falls off and `E(3)` raises. the
module holding such a class refuses to install anything at all below 3.13, so its
dict is present whatever is running. that is also why a decline never rests on the
widened dict: a refusal that held on 3.12 and not on 3.13 would be a wrong answer
on one of them.

`__dict__` itself stays unpublished on a class that has fields. a mapping naming
only what the layout had no room for would be an empty answer where the
interpreted class gives a full one, and that is quiet and wrong where the
refusal is at least loud. `vars(o)` is the visible consequence.

the invariant is checked once more where every attribute write passes, rather
than only where the fields are worked out: a write of a name nothing in the
receiver's layout chain holds declines instead of reaching for the dynamic form.

method dispatch has three speeds, chosen statically:

| receiver                                 | dispatch                                |
| ---------------------------------------- | --------------------------------------- |
| a `final` class, or a devirtualized call | a direct C call                         |
| a `sealed` base                          | a switch on the tag, then a direct call |
| anything else in the unit                | a vtable index                          |
| anything outside the unit                | `PyObject_GetAttr` + call               |

the vtable is a per-class array of function pointers laid out so that a subclass
extends its base's table, giving constant-time dispatch with no hashing

### traits, and multiple inheritance

full multiple inheritance does not have a fixed-offset layout, so it is not
supported for native classes. the supported subset:

- single inheritance from another native class
- any number of **trait** bases — classes with no instance layout of their own.
    a `protocol class` with only methods is a trait by construction, and
    basedpython uses protocols where python would reach for a mixin
- the non-native bases mypyc allows (`object`, `dict`, `BaseException` and its
    common subclasses)

trait method calls go through a small per-trait dispatch table. a class that
needs anything else becomes a **boxed class** and loses the layout optimizations
but keeps working

### how a type is built at import

a class on bases outside the module has three constructions open to it, and which
one applies is settled at import rather than when the C is written — only the
running interpreter knows what a base name resolved to. the bases are put through
`__mro_entries__` first, which is what a `class` statement does before anything
else, and then:

| construction                           | when                                       | what it costs                          |
| -------------------------------------- | ------------------------------------------ | -------------------------------------- |
| `PyType_FromSpecWithBases`             | every base's metaclass is `type`           | nothing — the slots are this module's  |
| `meta(name, bases, namespace, **kwds)` | anything else, if the class adds no fields | the instance layout, and slot dispatch |
| the interpreted definition             | a class with fields that cannot use a spec | the compiled methods go unused         |

a spec gives the type it builds `type` for a metaclass, so a base with any other
one is a conflict python rejects; and a spec has nowhere to put a class keyword.
`PyType_FromMetaclass` is not a way around either, because it refuses a metaclass
that overrides `__new__` — which `ABCMeta` does.

calling the metaclass is the general construction, and the methods go **in the
namespace it is handed** rather than onto the finished type. both halves of that
matter. `type.__new__` runs the same slot fixup a class statement does, so a
`__repr__` written in the class body fills `tp_repr` with no adapter of ours — and
every other dunder python knows comes with it. and a metaclass that reads the
namespace, such as an `ABCMeta` deciding which of the base's abstract methods this
class left abstract, sees what the class actually defines.

what it gives up is the layout: how big an instance is becomes the metaclass's
answer, so a class with fields of its own has nowhere to keep them and declines.
a method **decorator** is the other limit — it is applied to the finished type,
after the metaclass has already decided, so a class with one keeps the spec

a **class-level constant** is the same limit reached from the other side. its
value comes from the interpreted definition, which evaluated it at class
definition time, and module init copies it onto the finished type — which keeps
the object identical between the two builds, and under `type` is exactly right.
a metaclass that *makes* something of what the body wrote never sees it: an
`EnumType` handed a memberless namespace declares no members, and the copy then
lands them in the type's dict behind its back, so `FlagBoundary.STRICT` answers
while `_member_names_` is empty. feeding the interpreted twin's finished
attributes into the namespace instead does not rescue it either, because the
metaclass would build *new* members and every reference the module body already
took would still name the old ones. so a class with any constant keeps the spec,
and falls back to the interpreted definition where the bases deny it one

### what a method decorator is handed

a class body hands a decorator the **function** it defined, and a python function
carries a `__dict__`. `abc.abstractmethod` is the shape that depends on it: it
writes `__isabstractmethod__` onto its argument and hands that same object back.
a compiled method is a method descriptor, which takes no attributes at all, so
handing one straight to a decorator would raise where the interpreted twin does
not — the substitution, rather than the decorator, is what would fail.

so a decorated method is handed the descriptor with a `__dict__` on it: callable,
binding, and writable, which is the whole of what a decorator asks of a function.
only a *decorated* method is wrapped — an ordinary one keeps the descriptor and
the direct call. a method's decorators are then folded onto that one object and
the result written once, so the type never holds a half-decorated method, which
is also what a class body does

a class whose construction fell back to the interpreted definition is skipped: the
`def`s the fallback ran already carry their decorators, and applying them a second
time would wrap twice

### how many times a decorator runs

python evaluates a decorator **once**, where the definition stands. the twin is
what stands there, so the twin evaluates it — and module init then evaluates the
same decorator a second time over the native definition that replaced the twin's.
the name each definition ends up bound to is right either way, so nothing shows
but the side effect: `@register` puts two entries in its registry, `@count_them`
counts one function twice

for a module-level **function** and a module-level **class** the decorator is
therefore blanked out of the source the twin runs, so init's is the only
evaluation. blanking rather than cutting keeps every line where it was, which is
what a traceback through the twin quotes

that leaves a window: from the twin's `def` or `class` to the moment init reaches
it, the name holds a definition nothing has decorated yet. only the module's own
body can look — everything else runs after init — so a definition whose name that
body reads **declines** rather than be compiled and decorated twice. the reads
followed are the ones the body makes as it runs, plus, transitively, everything
held behind any definition it names: `TABLE = f()` reads directly,
`def g(): return f()` called at import reads just the same. an annotation counts
only where python evaluates one, so a module with
`from __future__ import annotations` may name a class in a signature freely

a **method's** decorator cannot be blanked the same way. it is not only a side
effect that would move: the class construction itself reads what the decorator
wrote, and `ABCMeta` is the case — it computes `__abstractmethods__` from the
namespace the body left, so taking `@abstractmethod` out of the twin empties that
set on every class whose construction falls back to the interpreted definition.

so the decorated method is carried *across* from the twin instead of being
decorated again. a method's decorators run **inside** the class body, which means
the body already holds the decorator's answer, and taking it is what makes the
single evaluation the only one. the rule is one rule rather than a branch: **take
the body's answer where there is one, apply the decorators where there is not** —
and the second case is not a second application, because the double is *caused* by
a body having run them, and a class with no interpreted `class` statement never
ran any

the price is that such a method is the interpreted one: a decorator is handed
whatever the body gave it, and there is no way to hand it the native method
without calling it again. an undecorated method is untouched and stays native,
which is where a compiled class's speed lives. `type(C.g)` then answers
`function`, which is also what python answers — so the change removed a second
divergence rather than adding one

### boxed classes and interpreted fallbacks

a construct with no native lowering is not a compile error. the module emits its
transpiled python source as a string constant, `exec`s it during module init,
and stores the resulting object in the module namespace. compiled callers reach
it through the `python` calling convention

this is what makes [total language coverage](plan.md#coverage-escape-hatches)
achievable on day one: an unsupported decorator, an exotic metaclass, or a
feature we have not lowered yet costs speed in that one place and nothing
anywhere else

a class left to the interpreted definition takes its base with it. the
interpreted `class` statement builds on whatever the base name resolves to, which
is the type this module emitted — and that is a subclass an emitted type cannot
have: its static type object refuses to be a base at all, and the direct method
call reads that refusal as proof no override exists. so a base an interpreted
class extends is left interpreted too, and `by compile --verbose` names the
subclass that caused it

#### when one class's refusal is the whole module's

a class that keeps its fields **past a base's instance** is the one shape with no
second construction to try. the storage is appended by the type spec, and the only
way to reach it is an offset into an instance that spec's type allocates — so
every compiled read and write of a field is a read of a layout the interpreted
definition does not have. its instances stop where the base's do, and the write
lands past the end of the object.

the spec can refuse. the base may be a heap type, whose deallocator picks what to
chain to from `Py_TYPE(self)` and comes straight back to ours; or carry a
metaclass, which a spec has no way to give the type it builds; or keep its
`__dict__` at an offset the appended layout has no room for. module init builds
these before it installs anything of its own precisely so that it can give up
there — the interpreted definition has already built the whole module, and leaving
it standing is a module that is merely slow rather than a mixture that is wrong.

but that refusal is only *necessary* where some compiled function would have read
one of these instances. where nothing does, the class alone falls back and every
compiled function in the module goes on standing. what counts as reaching into it
is deliberately wide, because missing one costs a wrong answer or a segfault where
an extra one costs only the whole-module refusal that was already the answer:

- any operation naming the class — a construction, a field read or write, a cell,
    a closure, the class object itself, a direct call to one of its methods
- any register, return or field **typed** as an instance of it
- any class naming it as a **base**. that reference is read while the other class's
    type is built, whether or not an instance of either is ever made, so it holds
    however little else runs

a generator method's state object and a nested function's closure environment are
each a class of their own, and each captures the `self` it was made from — so each
names the class exactly as any other reader would. counting those against it would
leave the narrower refusal firing on nothing, because almost every such class has
one. neither is in the namespace under any name and neither is built by anything
but the methods of the class it belongs to, so where that class has no type they
are never constructed: they go unbuilt with it rather than holding it.

`asyncio.unix_events` is the shape this is for. `_UnixSelectorEventLoop` stands on
a heap base from another module and can never be built, and it used to take
`PidfdChildWatcher`, `_UnixSubprocessTransport` and every compiled function in the
module down with it

#### the twin arrives compiled

parsing that source is most of what importing a compiled module costs — a
stdlib-sized module is milliseconds of it, enough that a compiled module could
import slower than the `.py` it came from. so the build asks the target
interpreter to compile the twin once and embeds the code object beside the
source. an import reads that instead: `argparse` goes from 6.6ms to 0.31ms and
`_pydecimal` from 11.2ms to 0.52ms

a code object is only good for the interpreter that wrote it, so the artefact
records two things about the one that did and the runtime checks both before
using it:

- the **bytecode magic**, cpython's own answer to the same question — it is what
    makes an upgraded interpreter regenerate a `.pyc` rather than misread one.
    handing 3.14 a code object 3.13 wrote segfaults the process, so this is not a
    tidiness check
- the **optimization level**, because `-O` takes `assert` out of the bytecode and
    `-OO` takes docstrings too. the twin has always been compiled by the importing
    interpreter, so `python -O` has always meant `-O` for it, and a code object
    compiled at the build's level would quietly stop meaning that

either mismatch sends the import back to the source, which is slower and is the
same program. a code object that passes both checks and then will not read is a
broken artefact rather than a mismatched one, and fails the import

## integers

`int` is `CPyTagged`: a pointer-sized word where an even value is a small
integer shifted left by one, and an odd value is a tagged `PyLongObject *`.
arithmetic is a fast path plus an overflow branch into the boxed path.
arbitrary precision is preserved

where a [range proves it](optimizations.md#ranges-from-the-type-system), an
integer is a plain `int8_t`…`int64_t` with no tag and no overflow branch. this
is the representation the numeric loops actually want, and the annotations that
select it are ordinary basedpython

one observable consequence, inherited from mypyc: an `int`-typed register loses
the distinction between `True` and `1`, because `bool` is an `int` subclass and
the tagged form has no room for it. covered in
[semantic deltas](plan.md#semantic-deltas)

## floats, strings, tuples

- **`float`** is an unboxed `double` wherever it is not stored in an
    `object`-typed slot. `.by`'s
    [exact float typing](../../features/no-number-promotions.md) means no
    int-check guards
- **`str`** stays a `PyUnicodeObject`, with direct access to its internal
    representation for length, indexing, and comparison. grapheme-level
    operations go to the rust segmenter
    ([intrinsics](optimizations.md#intrinsics))
- **`Character`** is a `str` subclass at the boundary, but a register holding one
    inside compiled code may be a `u32` code point plus a cluster length, boxed
    only when it escapes
- **fixed-length tuples** are unboxed C structs. `(count: int, total: int)` — an
    [anonymous named tuple](../../features/anonymous-named-tuple.md) — is two
    machine words in registers, not an allocation. variable-length `tuple[T, ...]`
    stays a real tuple object

## reference counting

BIR is written with **ownership as a property of each register**, with one
exception: **a parameter is borrowed**. the caller keeps ownership of an argument
for the duration of the call, so a native call site needs no retain and the
callee must not release a parameter on the way out.

> ⚠️ this was learned the hard way. releasing arguments in *both* the python
> wrapper and the callee's cleanup is a double-release that survives almost every
> test, because the caller usually holds its own reference to what it passed. it
> is fatal for a temporary — `f([1, 2])` segfaults — and silent for `f(1)`,
> because a small int is unrefcounted.

the exception has an exception: a parameter the body *reassigns* would have its
incoming value released by that write, so such a parameter is retained on entry
and released like any other register.

the refcount pass (pass 18) inserts the operations. the pass is not a heuristic — it
is a dataflow analysis over ownership, and the verifier rejects a function whose
paths are not balanced

three refinements over the baseline:

- **borrowed registers.** a register that provably lives no longer than an owning
    one holds no reference. mypyc infers these locally; a `local` parameter
    declares one that survives the call boundary
    ([escape analysis](optimizations.md#escape-analysis-that-crosses-calls))
- **immortals.** `None`, `True`, `False`, small ints and interned literals are
    immortal in cpython 3.12+, so their `IncRef` is a no-op the emitter drops
- **stack-allocated values** have no refcount at all
    ([stack allocation](optimizations.md#stack-allocation))

### free-threaded builds

`IncRef` / `DecRef` are abstract in BIR; the emitter picks the discipline for
the build being targeted. on a free-threaded build, a value proven thread-local
by `local` keeps biased (non-atomic) refcounting, and a `frozen` value can be
immortalized outright. the analysis that decides this is the same escape
analysis used for stack allocation, so free-threading support is mostly a
lowering choice rather than a second body of work — provided the abstraction is
taken now, which is why [technology](technology.md#free-threading-is-a-design-constraint-now-not-a-migration-later)
insists on it

## exceptions

the model is cpython's: raise sets the thread's current exception and the callee
returns an error value; the caller checks and propagates

what basedpython adds is that the check is *typed*
([error-path elision](optimizations.md#error-path-elision)):

| callee's `raises` set | after the call                                   |
| --------------------- | ------------------------------------------------ |
| `Never`               | nothing emitted                                  |
| a single class        | one sentinel test, one known handler target      |
| a union               | one sentinel test, then a switch on the tag      |
| `...`                 | today's behaviour — test and generic propagation |

`try` / `except` / `finally` lower to explicit CFG edges in pass 17. `finally`
blocks are duplicated along each exit path rather than implemented with a
saved-state trampoline, which is what lets the C compiler optimize the normal
path without the exceptional one weighing on it

### tracebacks

a compiled frame is not a python frame, so a naive traceback would skip it.
compiled functions push a lightweight entry recording the function and the
current `.by` line, so a traceback through compiled code shows the same file and
line numbers the interpreted build would

`by run` already rewrites tracebacks from transpiled `.py` lines back to `.by`
lines. compiled frames carry `.by` lines directly, so the same rendering path
serves both and the user sees no difference

## module initialization

a compiled module's `PyInit_` function, in order:

1. create the module object (multi-phase init, PEP 489)
1. create native type objects and populate vtables
1. run any `exec`'d [interpreted fallbacks](#boxed-classes-and-interpreted-fallbacks)
1. execute module-level statements as compiled code
1. publish the module namespace

module-level `let` bindings are [`Final`](../../features/modifiers.md), so they
are early-bound: a reference from a compiled function reads a static slot rather
than doing a namespace lookup. a non-`let` module global keeps late binding, and
the [performance docs should say so](plan.md)

installing a native definition writes over the name the fallback left behind,
which is that definition only while nothing rebound it. the singleton idiom
rebinds it:

```python
class _not_given:
    def __repr__(self):
        return '<not given>'

_not_given = _not_given()
```

the name holds an *instance* by the time init runs, so putting the class back
there is a wrong answer rather than a missing one. a definition whose name the
module body binds again afterwards is declined and stays interpreted. a binding
that comes *before* it is the ordinary forward declaration, which the definition
itself overwrites

### what else was holding the twin

installing a type over the name its twin left behind fixes that one name. it does
not fix anything *else* that captured the twin while the fallback body ran — and
by then the body has run in full, so plenty has.

the twin and its replacement are different objects, so every one of those holders
is stale, and the failure is silent: the value still works, it is just not the
object the name now means. an identity test is where it shows up.

```python
class Empty: pass

def f(ann=Empty):
    return ann is Empty       # python says True
```

the default was evaluated where the `def` stands — inside the fallback body,
before the type existed — so it held the twin, while `Empty` in the body reads the
name and gets the replacement. this is what made a compiled `inspect` render
`Signature()` as `() -> _empty`.

so the substitution is made everywhere a twin can still be held:

| holder                                        | when it is moved                              |
| --------------------------------------------- | --------------------------------------------- |
| a module-level name bound to the twin         | `By_RemapTwinAliases`                         |
| a class-level constant                        | `By_CopyClassConstant`, as the class is built |
| an attribute carried across                   | `By_AdoptTwinAttributes`                      |
| a retained interpreted definition's defaults  | `By_RemapTwinDefaults`, one call per handle   |
| a declined class's own methods and attributes | the same walk, one step in                    |

a value that merely **reaches** a twin — an instance whose type it is, a list
holding one — is not moved and cannot be. those stay as the body left them, and
that limit is the reason the rule is a substitution of *the twin itself* rather
than a deep rewrite.

## interoperating with interpreted code

the boundary is symmetric and both directions are guarded

**calling out** — into the stdlib, a third-party package, or an interpreted
module — uses the `python` convention: box the arguments, call, then check the
result against its declared type. that check is not new machinery; it is the
`generic-calls` / `returns` [soundness check](../../features/soundness.md) the
transpiled build already inserts. the pleasant consequence is that a wrong
typeshed annotation produces the same `TypeError` in both builds

**being called in** — from interpreted code — enters through the generated
`python` wrapper, which unboxes arguments and checks each against its parameter
type before the native body runs. that is the `parameters` soundness position

**subclassing across the boundary** is off by default: an interpreted class
cannot inherit a native class unless the class opts in, because the subclass
would not have the fixed layout. opting in keeps the native layout for native
callers and falls back to attribute lookup for the interpreted subclass

**pickle and copy** need `__init__` to be callable, or an explicit opt-in, for
the same reason mypyc does — the fixed layout must be initialized. a
`data class` satisfies this automatically

## debugging and inspection

- **`#line` directives** in the generated C point at `.by` source. gdb, lldb,
    `perf`, and the sanitizers all show basedpython lines and let you set
    breakpoints on them. the [sourcemaps](../sourcemaps.md) infrastructure
    already computes the mapping
- **`by compile --annotate`** writes an HTML view of each function: the `.by`
    source, the optimized BIR, and the emitted C, side by side, with a note at
    each point where an optimization was *not* applied and why. this is the
    single most useful tool for a user asking "why is this still slow", and
    mypyc's equivalent is one of its better-liked features
- **`by compile --emit=bir`** dumps textual BIR, which is also the snapshot
    format for the IR tests
- symbols are named from the qualified source name, so a profile reads like the
    program
