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
17.23x → 12.48x, both about a quarter. `fields`, `methods` and `generic` do not
move at all: a declared field is read and written at the offset it always was,
and the interpreted side still reaches it through a data descriptor, which wins
over the dict. `inherit` and `dot` were observed not to move either, but that was
before their inputs were built outside the timed region, so the observation is
against a row that no longer measures the same work.

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

`__dict__` names the whole of an instance's state, layout fields included. a
mapping naming only what the layout had no room for would be an empty answer where
the interpreted class gives a full one, so publishing one is what the refusal used
to protect against — and it is why the published mapping is not merely the overflow
dict.

the layout stays the storage and a published `__dict__` is a real `dict` that every
write to the instance updates, so a reference held across a later write cannot go
stale, and `isinstance(vars(o), dict)` holds for the code that tests it. the mapping
is installed in the instance's dict word, which is what lets python's own attribute
machinery put a stray name straight into it. a field read pays nothing for any of
this; a field write pays one test of that word.

two differences remain, both loud: `type(vars(o))` names the mapping rather than
`dict`, and `del o.__dict__[f]` for a *field* raises where python removes it,
because a layout field has no unbound state to return to.

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

but that refusal is only *necessary* where some compiled code that **still runs**
would have read one of these instances. so the question is not asked of one class
at a time: module init works out the largest family of classes it could leave
interpreted together, and refuses only where even that is not enough. a class in
that family is installed behind a test of every class it reaches, and where one of
them refused, none of them is installed — the whole family keeps the definitions
the module body built, and every compiled function in the module goes on standing.

a class is out of the family, and its refusal is the module's again, when
something outside the family reaches into it. what counts as reaching is
deliberately wide, because missing one costs a wrong answer or a segfault where an
extra one costs only the whole-module refusal that was already the answer:

- any operation naming the class — a construction, a field read or write, a cell,
    a closure, the class object itself, a direct call to one of its methods
- any register, return or field **typed** as an instance of it
- any class **naming it as a base**. that reference is read while the other class's
    type is built, whether or not an instance of either is ever made, so it holds
    however little else runs

the wait a class in the family is installed behind is transitive, for the reason
that makes an outside reader fatal in the first place: one compiled method calls
another's emitted body directly, with no type object in between. so installing a
class whose method calls a method that reads a class that stood down is as wrong as
reading it directly.

the base relation is followed **both** ways. a class left interpreted while its
base's emitted type took the base's name is standing on an orphaned copy of it, and
`isinstance` answers False against that name where python answers True with nothing
reported — so a whole inheritance family goes in or out together.

a generator method's state object and a nested function's closure environment are
each a class of their own. a state object holds the `self` it was made from, and an
environment holds it where a function nested in the method reads a name from further up
than the method — and holding it names the class exactly as any other reader would.
neither is in the namespace
under any name and neither is built by anything but the methods of the class it
belongs to, so where that class has no type they are never constructed: they are
part of the family rather than a reason to refuse.

`asyncio.unix_events` is the shape this is for. `_UnixSubprocessTransport` stands
on a heap base from another module and can never be built, and
`_UnixSelectorEventLoop._make_subprocess_transport` names it — so it used to take
`PidfdChildWatcher`, `_UnixDefaultEventLoopPolicy` and every compiled function in
the module down with it. the two event-loop classes stand on heap bases of their
own and cannot be built either, so the family stands down and the rest of the
module compiles.

what is **not** recovered is a module that cannot answer for its own classes at
all. `logging.handlers` writes fourteen handlers on `logging`'s own heap types, and
none of them installs — but the reason is `DatagramHandler.__init__` writing
`self.closeOnError`, a field its base declares, through the dynamic form. that is
the guard above, and while a module holds one the whole-module answer stands
however little else reads the class

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
python adds an entry for each frame an exception is raised in or passes through, and
a compiled function adds the same entry on the path its failure takes: the file, the
function and the line the failing operation was written on. the entry is cold — the
code object behind it is built the first time an exception passes that point and
kept, and only the frame it hangs off is made each time — so a function that does not
raise pays nothing for it. putting back an exception the frame already holds, as a
bare `raise` does, adds no entry, as in python

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
than doing a namespace lookup

a non-`let` module global keeps late binding, and a builtin is late-bound too — a
module that rebinds `str` is obeyed. so is a call to one of the module's own functions,
which goes straight to its native entry only while the name still holds the function the
module published, and while nothing about that function has been reassigned. each call site remembers what its name last
resolved to, and re-derives it whenever any namespace has been written to since,
which a dict watcher on those namespaces is what says. so the binding is still
looked up rather than assumed, and looking it up again is what a rebinding costs
rather than what every read costs

the builtins a compiled function falls back to are the ones the module it was
defined in was imported with, which is where python looks too — a caller standing
in a builtins namespace of its own does not redirect it

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

### checking that what was installed is what was reported

init ends by asking the finished namespace what is in it. for every class the
module publishes:

- the name holds the emitted type, and not the interpreted definition still
    sitting behind it. a class the module decorated is exempt: a decorator is
    arbitrary python handed the class and may publish anything
- `__bases__` holds, by identity, the type this module emitted for each base it
    emitted itself
- `_abc_impl` is the twin's own object, where the twin had one
- every name in the class's method table answers as a descriptor rather than as
    a `function`. a `function` there is the interpreted definition

a failure raises `ImportError` naming the class. it is not a warning, because
each of those is a wrong answer the program has simply not reached yet — a class
standing on an orphaned copy of its base answers `isinstance` False where python
answers True, and a type with an empty registry answers `issubclass(dict, Mapping)` False.

a class the module **stood down** is none of those. the layout guard above
leaves a class interpreted
where installing it would be wrong, and a construction that could not be rebuilt
hands the interpreted definition back to stand as the class — in both cases the
name holding that definition is the answer, not a defect, and the check records
it rather than raising.

recording is the other half, and it is what the check is for. `--annotate` says
which classes a module *meant* to compile; it cannot say which ones an import
stood a type under, and for most of this project's life the report was read as
though it could. so every published class writes a row — `installed`,
`interpreted`, or `twin` — to the file named by the `BY_INSTALL_CENSUS`
environment variable, and a build that compares those rows against the report is
holding the report to what ran. `scripts/native-sweeps/installcensus.sh` does
exactly that over the corpus.

the check runs once per module at import, never per call, and
`by compile --no-verify-install` leaves it out.

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

## where compiled code differs from python

each of these is a tradeoff made on purpose, and each names the shape of program
that can tell the two builds apart

### a module function rebound from outside

a compiled call to a function the same module defines goes straight to its native entry
while the name holds the function the module published, and through whatever the name
holds once it does not, so a function rebound, deleted or patched from another module
reaches the module's own compiled callers:

```python
def add(a: int, b: int) -> int:
    return a + b


def run(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        total = add(total, i)
        i = i + 1
    return total


# from another module
from unittest import mock

with mock.patch.object(mod, "add", lambda a, b: 7):
    mod.run(3)  # 7 on both
```

the question costs a load and a test on each such call, which is most of what a call
with a one-line body costs — `calls` takes 43% more instructions than a build that does
not ask — and a write to any name in the module costs a few dozen instructions more,
where the module is told which name it was. `bind-functions-early = true` in the
project's `[tool.ty.compile]` table makes the closed-world assumption instead: a compiled
caller goes on calling the function the module was compiled with, so a program that
rebinds, deletes or patches one of the module's functions, or reassigns its `__code__` or
its defaults, can tell. a call made from python still reaches whatever the name holds, and
the function's own python entry still binds from the defaults it holds:

```python
# built with `bind-functions-early = true`
mod.add = lambda a, b: 100
mod.add(1, 2)  # 100 on both
mod.run(3)  # python: 100; compiled: 3

del mod.add
mod.run(3)  # python: NameError; compiled: 3
```

a call that hands over a list the caller holds as a buffer cannot be made through a
replacement, for the reason a rebound `len` cannot be handed one, and raises
`RuntimeError` the same way:

```python
def total(xs: list[float]) -> float:
    out = 0.0
    i = 0
    while i < len(xs):
        out = out + xs[i]
        i = i + 1
    return out


def built(n: int) -> float:
    xs = [i * 0.5 for i in range(n)]
    return total(xs)


# from another module
mod.total = lambda xs: len(xs)
mod.built(3)  # python: 3; compiled: RuntimeError
```

### a rebound `len` and a list held as a buffer

a list of unboxed values that never leaves its function is held as a buffer
rather than as a `list`. `len` of it is a field read while `len` names the
builtin; a module that rebinds the name has what it rebound called instead, as
python does. a buffer has no `list` to hand over, though, and building one would
be a copy that the replacement could tell apart from the list python would have
passed, by identity or by writing to it. so that one call raises `RuntimeError`
instead:

```python
def total(n: int) -> float:
    xs = [i * 0.5 for i in range(n)]
    out = 0.0
    j = 0
    while j < len(xs):
        out = out + xs[j]
        j = j + 1
    return out


# from another module
mod.len = lambda xs: 1
mod.total(3)  # python: 0.0; compiled: RuntimeError
```

### how deep a recursion goes

a compiled call pushes a C frame rather than a python one, so a compiled module counts
the frames python would have pushed itself, against the interpreter's own recursion
limit: on a call that goes round a cycle of compiled calls, on a compiled method, dunder
or nested function the interpreter calls, and on each step of a compiled generator or
coroutine. a recursion raises `RecursionError` at the depth python raises it, and
`sys.setrecursionlimit` reaches it.

a C frame lives on the thread's stack, where a python frame lives on the heap, so the
stack is watched as well, and a compiled recursion raises `RecursionError` once three
quarters of the stack is used, whatever the limit says. a program that raises the
limit far enough can tell:

```python
def depth(n: int) -> int:
    if n == 0:
        return 0
    return depth(n - 1) + 1


# from another module
import sys

sys.setrecursionlimit(10**7)
mod.depth(500_000)  # python: 500000; compiled: RecursionError
```

a compiled call that goes round no cycle of compiled calls is not counted: it can only
be as deep as the module is long, and counting every call would put a cost on every
call. so a recursion reached through one reaches the limit one frame later for each
such call in front of it:

```python
def nested_depth(n: int) -> int:
    def inner(k: int) -> int:
        if k == 0:
            return 0
        return inner(k - 1) + 1

    return inner(n)  # this call is not counted


# from another module
sys.setrecursionlimit(100)
mod.nested_depth(98)  # python: RecursionError; compiled: 98
```

`==` and `!=` are one more such call. where the left operand's type has no comparison of
its own, python's answer is whatever the right operand's type says, and a compiled module
asks that type directly instead of going through `PyObject_RichCompare` — which leaves out
the recursion entry that function makes. that entry is against the C stack rather than the
frame count, so it is not what stops a recursion at the limit: a recursion through `__eq__`
stops at the same depth with the same message either way. what it costs is one level of
headroom, once, in a program that has raised the limit far enough for the stack to be the
thing that runs out. measured on 3.13 and 3.14, a recursion through `__eq__` reaches within
one level of the depth the same recursion reaches when the comparison is made to go the
long way round.

counting costs most on a body that does little but recurse — half again the
instructions of a plain `fib` — so `follow-recursion-limit = false` in the project's
`[tool.ty.compile]` table leaves the count out and watches the stack alone. a compiled
recursion then runs until the stack is close to running out, and a program that lowers
the limit, or catches `RecursionError` at a depth it expects, can tell:

```python
sys.setrecursionlimit(100)
mod.depth(200)  # python: RecursionError; compiled with the count off: 200
```

neither setting lets a recursion crash the process

### a function's defaults edited in place

python binds a missing argument from the function object's `__defaults__` and
`__kwdefaults__` as each call is made. a compiled module function is told when either is
reassigned, or its `__code__` is, and from then on every call to it — from python or from
compiled code — goes through the function object and the interpreted definition, given
the defaults the function holds then. an edit made *inside* the `__kwdefaults__` dict is
not a reassignment and nothing reports it, so until the function has been told of one the
defaults it was compiled with go on standing in:

```python
def scaled(a: int, *, scale: int = 1) -> int:
    return a * scale


# from another module
mod.scaled.__kwdefaults__["scale"] = 10
mod.scaled(2)  # python: 20; compiled: 2

mod.scaled.__kwdefaults__ = {"scale": 10}
mod.scaled(2)  # 20 on both, and every edit to that dict is seen from here on
```

watching the dict itself would put a test on every call for a program that almost never
edits it. a function compiled from a module is also published as a closure over its
native entry, so a `__code__` it is given has to close over as many names:
`mod.scaled.__code__ = (lambda a: a).__code__` raises `ValueError` where python takes it

what tells the function is a function watcher, and cpython hands a watcher every function
in the process rather than the ones it cares about. so once a compiled module that watches
its functions has been imported, each function python makes and frees anywhere — a lambda
or a nested `def` made on every pass of a loop, in any module — costs about 70 more
instructions on 3.13 and about 140 more on 3.14, whether or not it is ever edited

### a method is not a `function`

a class holds each method it compiled as a `by.method_descriptor` rather than a
`function`, so `isinstance(C.read, types.FunctionType)` is `False`, and its type is named
`method_descriptor` where python's is named `function`. it binds as a function does, and
what a function says about its `def` — `__module__`, `__qualname__`, `__doc__`,
`__defaults__`, `__kwdefaults__`, `__code__`, `__globals__` and the annotations — is the
interpreted definition's, so `inspect.signature`, `typing.get_type_hints` and
`functools.wraps` read the answers python gives. what it does not answer is
`__closure__`, whose cells would belong to the interpreted definition's class rather than
the compiled one, and an attribute written onto it is kept without changing what a call
runs:

```python
class Scale:
    def get(self, k: int = 3) -> int:
        return k


# from another module
mod.Scale.get.__closure__  # python: None; compiled: AttributeError
mod.Scale.get.__defaults__ = (10,)
mod.Scale().get()  # python: 10; compiled: 3
```

### a dunder a slot answers

a dunder python calls through a type slot — `__init__`, `__eq__`, `__len__` and the rest
— is not one of those methods. the class holds the `wrapper_descriptor` cpython makes for
the slot, which is what `object.__init__` is too, so it says nothing about the `def` it was
compiled from: it has no `__annotations__`, `__defaults__` or `__code__`, and what reads
those reads the slot's own description instead. `inspect.signature` of the class itself
finds no signature at all:

```python
class Point:
    def __init__(self, x: int, y: int = 0) -> None:
        self.x = x
        self.y = y


# from another module
mod.Point.__init__.__annotations__  # python: {'x': int, 'y': int, 'return': None}; compiled: AttributeError
typing.get_type_hints(mod.Point.__init__)  # python: {'x': int, 'y': int, 'return': NoneType}; compiled: {}
inspect.signature(mod.Point.__init__)  # python: (self, x: int, y: int = 0) -> None; compiled: (self, /, *args, **kwargs)
inspect.signature(mod.Point)  # python: (x: int, y: int = 0) -> None; compiled: ValueError
```

publishing a method in the slot's place would answer the first three, but cpython then
stops giving the slot to an interpreted subclass, which makes every construction through
one slower, and `inspect.signature(mod.Point)` would answer `(*args, **kwargs)` rather
than raise. answering the class's signature through a `__signature__` of its own would ignore
what the caller asked for: `inspect.signature(mod.Point, eval_str=True)` would hand back
the annotations a `from __future__ import annotations` module left as strings

### one module object for the whole process

a compiled module keeps its namespace, and every memo of a name in it, in state
the whole process shares, so it can stand behind one module object and no more.
importing it again in the same interpreter hands back the module the first import
made, as importing a C extension that keeps no per-module state does. python would
build a second module and run its body again:

```python
import sys
import mod

del sys.modules["mod"]
import mod as again

again is mod  # python: False; compiled: True
```

importing it in a second interpreter — a subinterpreter, legacy or isolated — is
refused with an `ImportError`, since there is no second copy of that state to give
it:

```python
import _interpreters
import mod

sub = _interpreters.create("legacy")
_interpreters.exec(sub, "import mod")  # python: imports; compiled: ImportError
```

### a generator is not python's `generator`

a compiled generator, coroutine or async generator is an object of a type of its
own, named after the function it belongs to, rather than an instance of python's
`generator`, `coroutine` or `async_generator`. it answers every step python's does —
`next`, `send`, `throw`, `close`, `await`, `async for`, running `finally` blocks when
it is closed, collected or dropped — but it carries none of the introspection a
frame object gives python's:

```python
import inspect
from collections.abc import Iterator


def counted(n: int) -> Iterator[int]:
    yield n


g = counted(1)
type(g).__name__  # python: 'generator'; compiled: 'counted$gen'
inspect.isgenerator(g)  # python: True; compiled: False
g.gi_frame  # python: a frame; compiled: AttributeError
```

the same holds for `gi_running`, `gi_code`, `gi_yieldfrom`, `cr_frame`, `cr_await`,
`ag_frame`, and the object's own `__name__` and `__qualname__`. a message python words
after the type's name names the compiled type instead, so
`type(g)()` refuses with `cannot create 'mod.counted$gen' instances` where python
says `cannot create 'generator' instances`

### the three-argument `throw` through an interpreted generator

a compiled frame suspended in `yield from` or `await` hands a `throw` on to the
iterator it is waiting on, as python does. python hands a generator or coroutine of
its own the arguments of the deprecated `throw(type, value, traceback)` form without
calling its `throw` method, which would warn about the deprecated form a second
time. a compiled frame cannot reach that generator any other way, so it builds the
exception out of the three arguments and hands the generator that one exception.
the generator builds the same exception itself when it is the frame that takes the
throw, so the two builds differ only where it is waiting on something in turn — an
iterator with a `throw` of its own is then handed one argument rather than three:

```python
# an interpreted module
class Inner:
    def __iter__(self):
        return self

    def __next__(self):
        return 1

    def throw(self, *args):
        return len(args)


def middle():
    yield from Inner()


# the compiled module
def outer(make: Callable[[], Iterator[object]]) -> Iterator[object]:
    yield from make()


g = outer(middle)
next(g)
g.throw(ValueError, ValueError("x"), None)  # python: 3; compiled: 1
```

for the same reason, arguments the exception cannot be built from — a traceback
argument that is not a traceback, say — are refused at the compiled frame, where
python hands them on unchecked to whatever the interpreted generator is waiting on

### a traceback entry's frame

a traceback through compiled code names the same files, functions and lines python's
does, but the frame each compiled entry hangs off was made for the entry rather than
being the frame the function ran in. its code object has no bytecode, so it carries no
column positions and a formatted traceback prints no `~~^^` markers under the line;
the frame holds no locals; and its code object's `co_firstlineno` is the entry's own
line and `co_qualname` its bare name:

```python
import traceback

try:
    mod.parse("")
except ValueError as e:
    tb = e.__traceback__.tb_next
    tb.tb_frame.f_locals  # python: {'text': '', ...}; compiled: {}
    traceback.print_exception(e)  # python underlines the failing call; compiled does not
```

### writing onto a class nothing extends

a class the module neither extends nor decorates, with no base and no written `__new__`,
is emitted as an immutable type, which is what lets a call reach its compiled methods
directly. python writes to a class through its type, and an immutable type refuses a
write, a replacement or a `del` of an attribute with `TypeError` — so a class written to
once the module has been imported, from a function of its own or from another module,
raises where python's class takes the write. what the module body writes after the
`class` statement lands on the interpreted definition while the module is still being
built, and the compiled class carries it:

```python
class Sealed:
    def read(self) -> int:
        return 1


Sealed.early = lambda self: 2  # both: carried onto the compiled class


def patch() -> None:
    Sealed.late = lambda self: 3


patch()  # python: None; compiled: TypeError: cannot set 'late' attribute of immutable type


# from another module
mod.Sealed.other = 5  # python: stored; compiled: TypeError
del mod.Sealed.read  # python: deleted; compiled: TypeError
```

a class the module extends, or one the source decorates, is a mutable type and takes the
write as python's does

### an interpreted subclass whose `__init_subclass__` does not chain up

a compiled class keeps an `int` field unboxed, and a field no `__init__` has set holds a
value no `int` can have. the class's allocator writes that value, and a class made by a
`class` statement or `type(...)` is given python's generic allocator instead of its
base's. so the compiled class publishes an `__init_subclass__` that hands every subclass
its allocator as the subclass is made, and then goes on up the chain as a written one
calling `super().__init_subclass__(**kwargs)` does. that entry is in the class's own
dict, where python's class has none:

```python
class Root:
    pass


class Made(Root):
    def __init__(self) -> None:
        self.count = 1


# from another module
"__init_subclass__" in vars(mod.Made)  # python: False; compiled: True
```

a subclass that writes an `__init_subclass__` of its own and does not call
`super().__init_subclass__()` keeps the hook from its own subclasses. one of those gets
the allocator from the first instance built through the compiled class's `__new__`, and
until then an unset `int` field on an instance `object.__new__(cls)` built reads as `0`:

```python
# from another module
class Quiet(mod.Made):
    def __init_subclass__(cls) -> None:
        pass


class Below(Quiet):
    pass


object.__new__(Below).count  # python: AttributeError; compiled: 0
```

### a value nothing verified, with its soundness check turned off

the interpreted build checks a value whose type nothing verified where it meets a
declared type — an `Any`, a generic call's result, an element read out of a container —
and a compiled module makes each of those [soundness checks](../../features/soundness.md)
in the same place, against the same classes, raising the same `TypeError`. a check the
value's own representation already proves is not made again: an element unboxed out of a
`list[int]` passed the unbox's own test, which raises that `TypeError` itself.
`--soundness` turns positions off for both builds, and a value held as an `object` — a
return handed back as one, an optional, a container, an instance of a class the module
does not lay out — then goes unchecked in both.

what no position turns off is the test a compiled module needs to hold a value at all. a
local, a parameter or a field declared `int`, `float`, `bool` or `str`, or as an instance
of a class the module lays out, holds that representation, so a value that is not one
raises `TypeError` there where the interpreted build goes on holding it:

```python
# built with `--soundness none`
from typing import Any


def held(v: Any) -> object:
    a: int = v
    return a


held("x")  # python: "x"; compiled: TypeError
```

a name a class pattern binds from such a field is held the same way. `isinstance` believes
an object's `__class__`, so an object claiming a class it was not made by matches that
class's pattern and has its attributes looked up, and one holding something else under
a field the class keeps as an `int` raises where python binds it:

```python
class Point:
    def __init__(self, x: int) -> None:
        self.x = x


class Claims:
    __class__ = property(lambda self: Point)
    x = "five"


def read(v: object) -> object:
    match v:
        case Point(x=x):
            return x
    return None


read(Claims())  # python: "five"; compiled: TypeError
```

### a complex power held where a `float` is declared

python's `**` gives a complex number for a negative base to a fractional power, and the
checker types `float ** float` as `Any` for that reason. a compiled `**` that could answer
with one is python's own, through the object protocol. a place declared `float` holds an
unboxed double, though, so storing a complex answer there raises `TypeError` where python
stores it:

```python
def root(a: float) -> float:
    x: float = a**0.5
    return x


root(-4.0)  # python: (1.2246467991473532e-16+2j); compiled: TypeError
```

an augmented assignment is not one of these: `x **= 0.5` binds the power's result, and the
local is chosen to hold whatever that can be

### what a closure keeps alive

python gives each name a closure captures a cell of its own, so a closure keeps alive the
values of the names it reads and nothing else. a compiled frame keeps every name its
closures capture in one environment they all share, so a closure keeps alive what its
siblings captured too, and a lambda made inside a generator holds the generator's whole
state. no value changes; what can tell is a finalizer or a weak reference, which sees the
release later:

```python
class Big:
    pass


def pair(big: Big):
    small = 1

    def a() -> int:
        return small

    def b() -> Big:
        return big

    return a


def gen(big: Big):
    small = 1
    yield lambda: small


# from another module
import gc, weakref

big = Big()
alive = weakref.ref(big)
kept = pair(big)  # or `next(gen(big))`
del big
gc.collect()
alive() is not None  # python: False; compiled: True, for as long as `kept` lives
```

a cell for each captured name would close this, and that is a different shape for every
closure a compiled frame makes

### a nested function's annotations

a compiled nested function answers `__annotations__`, `__type_params__` and, from 3.14,
`__annotate__` with what python evaluates from the interpreted definition's annotations,
over the values the enclosing frames hold for the names in them. `inspect.signature` and
`typing.get_type_hints` read those, `__code__`, `__defaults__` and `__kwdefaults__`, and
answer as they do for the interpreted definition. what can still tell the two apart:

```python
def outer(kind: type):
    def inner(y: kind) -> kind:
        return y

    return inner


f = outer(int)
```

- on 3.13 the annotations are evaluated when they are first asked for, as 3.14 does, over
    the values the enclosing names held where the `def` stood. an annotation that raises or
    has an effect does so at that first read rather than at the `def`, and never where
    `__annotations__` is written before it is read — see below
- on 3.13 a name the annotations read from an enclosing frame that nothing has bound yet
    raises `NameError` naming a free variable, where python raises `UnboundLocalError`
- the closure holds the enclosing names its annotations read, as a 3.14 `__annotate__`
    does, so on 3.13 a value the enclosing frame binds to one of them after the `def` lives
    as long as the closure, and the value each held at the `def` lives until the annotations
    are first read
- on 3.14 `f.__annotate__` is made from the values the enclosing names hold when it is
    read, and is a new function each time: a name rebound between reading it and calling it
    is seen with its earlier value, and `f.__annotate__ is f.__annotate__` is `False`
- an annotation that reads a type parameter of an enclosing function
    (`def outer[T](): def inner(x: T)`) where the module does not compile annotations as
    strings, or that names a private name inside a class, raises `RuntimeError` when the
    annotations are asked for
- `__defaults__` and `__kwdefaults__` can be read and not written, and `__code__` is the
    interpreted definition's code object, which the compiled function does not run

evaluating the annotations where the `def` stands is a python call, which made a closure
with annotations cost about 3,400 more instructions to make on 3.13 than one without —
the price of a `def` inside a loop. what the `def` takes now is the values alone, so an
annotation with an effect shows when it runs:

```python
def note(label: str) -> type:
    print(label)
    return int


def outer():
    def inner(y: note("y")) -> None:
        pass

    return inner


f = outer()  # python 3.13: prints `y`; compiled: prints nothing
f.__annotations__  # compiled: prints `y`
```

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
