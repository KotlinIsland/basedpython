# local lifetimes

> **status: partially implemented.** `local` and `once` parameters parse, lower
> to clean python, and are enforced by ty today — `escaping-local` (with `once`
> treated as a borrow), `once-not-called`, `once-called-twice`, and
> `escaping-loop-variable` — as are `local` / `once` on a callable type's own
> parameters, which constrain the trailing lambda block filling it. still design
> sketches: the opt-in `once` runtime guard and the `T_{x}` lifetime notation.
> those sections are marked below

python is garbage collected: every value lives as long as something references
it, and no reference is ever "too old". that safety net also erases a whole
class of intent. you cannot say *this callback must run exactly once*, or *this
reader is only valid for the duration of this call*, or *the sequence i hand
back is a view into the buffer you gave me and dies with it*. so those
contracts live in docstrings, and their violations live in production

basedpython adds a **static escape analysis** that lets you state them in the
signature and checks them at transpile time. three cooperating constructs feed
it:

- `local` — a parameter (or callback parameter) that **must not escape** the
    call it is bound in — a borrow
- `T_{x}` — a type whose validity is **tied to the lifetime of `x`** — a view
- `once` — a callback that **must be called exactly once** — a linear value

none of this changes what the program does at runtime. like
[`abstract` and `override`](modifiers.md), the markers are compile-time-only:
they are erased in the lowered python, and everything they promise is enforced
by ty's diagnostics before a line runs. `once` optionally emits a runtime guard,
in the same spirit as [soundness checks](soundness.md) and
[checked casts](checked-cast.md)

## `local` parameters

a `local` parameter is one the callee may *use* but may not *keep*. it borrows
the value for the length of the call and no longer:

```by
def f(local fn: () -> None):  # fn is only valid throughout the call to f
    fn()                      # ok — used within the call
```

transpiles to (the marker is stripped):

```python
def f(fn: Callable[[], None]) -> None:
    fn()
```

the check is on the *callee*. inside `f`, the value bound to a `local` parameter
must not outlive `f`'s activation. these are the ways a value escapes, each
reported as `escaping-local`:

| escape route                       | example                                   |
| ---------------------------------- | ----------------------------------------- |
| returned                           | `return fn`                               |
| stored on a global / nonlocal      | `_registry = fn`                          |
| stored on a longer-lived object    | `self.cb = fn`                            |
| put in a container that escapes    | `handlers.append(fn)`                     |
| captured by a closure that escapes | `return lambda: fn()`                     |
| handed to a non-`local` parameter  | `schedule(fn)` — `schedule` might keep it |

and these uses are always fine — a borrow is fully usable, it just cannot be
retained:

- calling, reading, indexing, iterating, or calling methods on it
- assigning it to a binding that does **not** outlive the call (an ordinary
    local)
- passing it on to another `local` parameter — the borrow is re-lent, not
    extended (see [re-borrowing](#re-borrowing))
- constructing something from it (`str(xs)`, `list(xs)`) — a constructor is
    read as consuming what it is given rather than retaining it

the last row of the table is the one that bites in practice. no stdlib signature
is annotated `local`, so an ordinary function call escapes even when the callee
plainly does not retain anything:

```by
def f(local xs: list[int]) -> None:
    print(len(xs))    # error: escaping-local
    print(sum(xs))    # error: escaping-local
    print(str(xs))    # fine — a constructor
    print(xs[0])      # fine — indexing
    for x in xs: ...  # fine — iterating
```

`len` and `str` differ only in that one is a function and the other a class,
which is not a distinction the reader has any reason to expect. this is the
largest rough edge in the current implementation: deciding it properly needs
`local` annotations through the stdlib, and guessing which callees retain their
arguments is exactly the kind of heuristic this feature exists to replace.

### re-borrowing

passing a `local` value to another `local` parameter is always allowed: the
second callee is under the same no-escape obligation, so the value still cannot
outlive the original call:

```by
def log_each(local items: Iterable[str]): ...

def f(local items: Iterable[str]):
    log_each(items)   # ok — re-borrowed, still cannot escape
```

passing it to a *non-*`local` parameter is the escape — that callee is free to
retain it, so the borrow would be laundered into an unbounded lifetime. the fix
is to mark the downstream parameter `local` too, or to hand over a copy (see
[severing a tie](#severing-a-tie))

## lifetime-bound types

> **planned** — the `T_{x}` notation and its `dangling-lifetime` check are not
> implemented yet; this section is a design sketch

a borrow answers "can the callee keep this?". it does not answer the dual
question: "can the callee hand me back something that secretly *is* this?".
consider a function that borrows a buffer and returns its lines lazily:

```by
def f(local x: BufferedReader) -> Sequence[str]:
    ...
```

the returned `Sequence[str]` is a *view* over `x` — reading from it reads from
the buffer. so it is only valid while `x` is. but the signature above does not
say that, and nothing stops a caller from draining the sequence after the
buffer is gone. we need to tie the return's lifetime to `x`:

```by
def f(local x: BufferedReader) -> Sequence[str]_{x}:
    ...
```

`T_{x}` reads "`T` scoped to `x`": a value of type `T` whose validity is bounded
by the binding `x`. it is stripped in the lowered python — `Sequence[str]_{x}`
becomes plain `Sequence[str]` — and carries its meaning only through the checker

there is no separate lifetime variable to declare: you name the binding
directly. the lifetime *is* the parameter. `self` is available inside
methods, which covers the common "a view into me" case:

```by
class Buffer:
    def view(local self) -> memoryview_{self}:
        ...
```

### at the call site

a lifetime tie propagates to the result. wherever `f(a)` is called, the result
is recorded as a view of the specific argument passed for `x`, and inherits its
lifetime:

```by
def read_lines() -> list[str]:
    reader = open_reader()
    lines = f(reader)     # lines is a view of reader
    return list(lines)    # ok — materialized before reader is dropped

def bad() -> Sequence[str]:
    reader = open_reader()
    return f(reader)      # error[dangling-lifetime]: result outlives `reader`
```

in `bad`, the returned view would outlive `reader`, which is local to `bad` —
`dangling-lifetime`. in `read_lines`, `list(lines)` copies the data out before
`reader` ends, [severing the tie](#severing-a-tie), so the `list[str]` is
free-standing and safe to return

### multiple lifetimes

a value can be a view of more than one binding — it is valid only while **all**
of them are, so it dies with the first to end:

```by
def zip_views(local a: Buffer, local b: Buffer) -> Sequence[bytes]_{a, b}:
    ...
```

### severing a tie

a lifetime tie is a claim about aliasing, so anything that *copies the data out*
ends it. the checker recognizes a tie as severed when the value's static type is
one that cannot alias its source:

- an **immutable scalar** read out of a view — `int`, `str`, `bytes`, `bool`,
    `float`, `None`, an [enum](enums.md). `lines[0]` is a plain `str`; it holds
    no reference back to the buffer, so it is free-standing even though `lines`
    is a view
- an **eager materialization** through a known builtin constructor — `list(v)`,
    `dict(v)`, `set(v)`, `tuple(v)`, `frozenset(v)`, `bytes(v)`, `bytearray(v)`,
    `str(v)`, or a comprehension that copies elements

what stays tied are the things that genuinely alias: declared `T_{x}` results,
`memoryview`, iterators and generators, and user view-types you have annotated.
the rule of thumb — **scalars and copies sever, views and lazy wrappers do
not** — is the same soundness argument the [soundness checks](soundness.md) rest
on: a value that carries no back-reference to the resource cannot dangle

## `once` callbacks

`once` marks a callback parameter that **must be called exactly once** on every
path that completes the function normally — a linear value. it is the tool for
completion handlers, one-shot continuations, and any "you must, and must not
forget to, and must not do it twice" protocol:

```by
def with_transaction(once commit: () -> None):
    do_work()
    commit()          # exactly once — ok
```

the static check counts direct calls of `commit` with their control-flow
context, and reports two failures:

- `once-not-called` — the callback is never called (it is not mentioned anywhere
    in the body). a callback that is passed on to another `once` parameter is
    left alone, since that receiver must call it exactly once
- `once-called-twice` — two **unconditional** calls, or a call inside a **loop**
    (which may run any number of times)

`once` is a **borrow**: a callback you can only guarantee to run exactly once is
one you have not let escape your control. so `once` is `local` plus the count
obligation — it is escape-checked exactly like a `local` (returning it, storing
it, or binding it to a `global` is `escaping-local`), and it may only be passed
on to another **`once`** parameter. handing it to a plain `local` (which could
call it zero or many times) or a non-borrow parameter would drop the guarantee,
so both are rejected:

```by
def keep(once cb: () -> None): cb()
def borrow(local cb: () -> None): cb()

def f(once done: () -> None):
    keep(done)      # ok — the obligation is preserved
    borrow(done)    # error[escaping-local] — a `local` need not call it once
    return done     # error[escaping-local] — cannot escape at all
```

because a `once` block is confined to its call, it is also safe to capture a loop
variable in a [trailing-lambda block](trailing-lambdas.md) bound to a `once` (or
`local`) callee — the block runs synchronously, so the variable still holds this
iteration's value. a block bound to a non-borrow callee that captures a loop
variable is the late-binding trap, reported as `escaping-loop-variable` (the
type-aware companion to ruff's `B023`)

the check is deliberately conservative — it flags only what it can prove, so it
never fires on correct code. two calls in mutually-exclusive branches are one
call on every path and pass:

```by
def f(once done: () -> None, ready: bool):
    if ready:
        done()
    else:
        done()      # ok — exactly one call on every path
```

a single-branch conditional (`if ready: done()`) may skip the call, but the
static check does not flag it — proving "skipped on some path" is what the
[runtime guard](#runtime-guard) is for. a call inside a loop *is* rejected
(`once-called-twice`), matching the rule that the loop may run more than once

### borrowed callback arguments

the two features compose through the callback's *own* signature. a callback type
can mark its parameters `local`, which says: when the callee invokes the
callback, the value it passes in is local to the callee, and the callback body
may not leak it. this is the flagship case, using a
[trailing lambda block](trailing-lambdas.md) as the callback:

```by
def f(once fn: (local Resource) -> None):  # fn: called once; its argument is local to f
    with acquire() as resource:
        fn(resource)                       # resource is local to this call

let result: Resource
f:
    result = it   # error[escaping-local]: `it` is local to `f`, cannot escape the callback
```

`(local Resource) -> None` is a callable type with a single `local` parameter. the
trailing block becomes `fn`, and its implicit parameter [`it`](trailing-lambdas.md)
binds that `local` position — so inside the block, `it` carries `f`'s lifetime.
`print(it)`, `it.read()`, or handing `it` to another `local` parameter are all
fine; assigning it to the outer `result` is the escape — a block's assignments
[write through](trailing-lambdas.md) to an enclosing binding, so the value would
outlive the call. the parameter may be named for clarity, and that spelling also
covers a type a bare modifier cannot precede:

```by
def f(fn: (local resource: Resource) -> None): ...
def g(fn: (once cb: (int) -> None) -> None): ...
```

the modifier is only read when a *name* follows it, so a bare modifier before a
parenthesized type (`(local (int) -> None)`), a string forward reference, or a
starred type is not one — `once (x)` and `local (y)` are an ordinary call and a
parenthesized name everywhere else, and a parameter list is not always
distinguishable from a value tuple when the modifier is read. naming the
parameter removes the ambiguity. for the same reason there is no bare `(local )`
spelling: it is the same token stream as `(local)`, the one-parameter list whose
parameter has the type named `local`

`once` in a callable type carries the whole obligation, not just the borrow: the
block filling that callable must call the marked parameter exactly once, and is
reported with `once-not-called` / `once-called-twice` if it does not.

the borrow is a constraint on the callback's *implementation*, not on its
callers, so it does not change assignability: a `(local int) -> None` and an
`(int) -> None` remain mutually assignable, and passing an ordinary function
where a borrowed callback is expected is not an error. what is checked is the
body of a block written in that position. a callee whose callback shape cannot
be inspected leaves the block unconstrained, as everywhere else here

### runtime guard

> **planned** — only the static `once` check ships today; the runtime guard
> below is a design sketch

the static check is conservative — it approves only what it can prove. for the
cases it cannot (a call buried behind a helper, a hand-rolled event loop), an
opt-in runtime guard turns "exactly once" into an enforced runtime invariant,
covering exception paths the static analysis deliberately leaves alone:

```python
class _OnceGuard:
    def __init__(self, fn):
        self._fn = fn
        self._called = False
    def __call__(self, *args, **kwargs):
        if self._called:
            raise TypeError("once callback called more than once")
        self._called = True
        return self._fn(*args, **kwargs)
    def _ensure_called(self):
        if not self._called:
            raise TypeError("once callback was never called")
```

with the guard enabled, `with_transaction` lowers to wrap the callback and
assert in a `finally`, so both "twice" and "never" raise — the latter even when
the body exits by raising:

```python
def with_transaction(commit):
    commit = _OnceGuard(commit)
    try:
        do_work()
        commit()
    finally:
        commit._ensure_called()
```

## grammar

`local` and `once` are parameter-position keywords, parsed in the same slot as
the visibility and binding modifiers:

- `local` precedes a function/method parameter (`def f(local x: T)`) or a
    parameter inside a [callable type](callable.md) (`(local T) -> R`,
    `(local name: T) -> R`). in a callable type it is only read when a name
    follows it, so `(local)` stays the one-parameter list whose parameter has the
    type named `local`, and a type that does not start with a name has to be
    reached through the named form
- `once` precedes a callback parameter (`def f(once fn: () -> R)`). it is only
    meaningful on a callable-typed parameter; a planned `once-on-non-callable`
    check will flag `once` on a non-callable (today such a parameter simply reads
    as never-called)
- both may apply to one parameter as `once local fn` when a callback both must
    run exactly once and must not itself be retained
- `T_{x}` is a postfix on any type expression, where `x` names a parameter (or
    `self`) in the enclosing signature. an unknown name is `unknown-lifetime`

all three are `.by`-only. a `.py` file that uses them is a parse error, exactly
as with the other syntax extensions

## lowering

the markers carry no runtime meaning (except the `once` guard) and are erased in
a single forward pass:

| basedpython                  | python                                        |
| ---------------------------- | --------------------------------------------- |
| `def f(local x: T)`          | `def f(x: T)`                                 |
| `-> Sequence[str]_{x}`       | `-> Sequence[str]`                            |
| `def f(once fn: () -> None)` | `def f(fn: Callable[[], None])` (static-only) |
| `(local T) -> None`          | `Callable[[T], None]`                         |

the callable-arrow half of the last two rows lowers through the existing
[callable](callable.md) transform; `local`/`once` are removed before it runs.
because nothing in the lowered python distinguishes a `local` parameter from an
ordinary one, the markers do not survive a round-trip — the
[reverse transform](../development/reverse-transforms.md) has no lowered shape to
detect and cannot reconstruct them, the same as any other erase-only marker

## configuration

the escape, lifetime, and `once` **diagnostics** are always available — they are
pure static analysis with no codegen, so they run on every check and, like any
ty rule, can be downgraded or silenced per project or per line

the `once` **runtime guard** is opt-in, mirroring `--no-checked-cast`: pass
`--once-checks` to `by run`, `by build`, or `by transpile` to emit the
`_OnceGuard` wrapper, or leave it off for a zero-overhead static-only build. the
type is identical either way

## composition

- with [trailing lambdas](trailing-lambdas.md) — the `it` parameter of a block
    inherits the `local` marker from the callback type, which is what makes
    `escaping-local` fire on `result = it`
- with the [callable arrow](callable.md) — `local` sits inside the arrow's
    parameter list and lowers with it
- with [soundness checks](soundness.md) and [checked casts](checked-cast.md) —
    lifetimes are the static complement to those runtime guards: soundness
    validates *what* a value is, lifetimes validate *how long* it may be used.
    an explicit copy that severs a tie is exactly a runtime materialization the
    soundness checker already understands

## limits

the escape analysis is intraprocedural and **best-effort** — it reasons within
one function body and across signatures, never by inlining a callee. it flags an
escape only where it can see the local reach the exit directly: a bare name (or
one held in a surface container / ternary / boolean) that is returned, stored on
a parameter-rooted or `global` / `nonlocal` target, or handed to a resolvable
non-`local` parameter. that leaves it biased toward **false negatives**, not
false positives — three consequences:

- an **unannotated** return is assumed free-standing. `def f(local x: R) -> V`
    with no `_{x}` is taken at its word — if the body actually returns a view of
    `x`, that is a missing annotation, reported at the `return` inside `f`, not
    at `f`'s call sites
- a local **captured by a closure** (`return lambda: fn()`) or routed through an
    **opaque call** that retains it is not currently detected — the `return   lambda: fn()` row above is the intended contract, not yet enforced. tightening
    these toward soundness is future work; the escape hatch meanwhile is an
    explicit copy (which severs the tie) or a suppression comment
- an escape through a callee is caught only when the callee's signature is
    **resolvable**. `schedule(fn)` is flagged when `schedule`'s parameter can be
    inspected and is not `local`; an opaque callee is left alone

`once` is checked on paths that **return normally**. a path that propagates an
exception is exempt from the static count — any line in python may raise, so
requiring the call on every exceptional path would reject almost everything. the
runtime guard closes that gap for code that needs a call-on-error guarantee; the
idiomatic static equivalent is `try` / `finally`

## open questions

- **notation** — `T_{x}` is the leading spelling; a keyword form (`T for x`,
    `T @x`) may read better in nested positions like `Iterator[Line for x]`
- **`once?`** — an at-most-once (affine) relaxation, following the language's
    `?`-means-relaxed convention ([`cast?`](checked-cast.md),
    [`?.`](optional-chaining.md), [`??`](none-coalesce.md)): may be skipped,
    never called twice. it is the natural type for a consuming callback that a
    fast path can decline to run
- **`local` locals** — allowing `local y = expr` on an ordinary binding to opt a
    local variable into escape checking, not just parameters
- **lifetime elision** — whether a function with exactly one `local` parameter
    should tie an unannotated return to it automatically (it trades explicitness
    for brevity, against basedpython's escape-by-default stance)
