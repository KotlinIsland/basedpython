# basedpython-ui support

basedpython-ui is a compose-style ui library: a `@composable` function describes a piece of ui, and re-runs whenever one of the observables it read changes. the type checker understands that model, so the mistakes it invites are caught where they're written rather than at runtime.

```by
from basedpython_ui import composable, state, Button, Text

@composable
def Counter():
    let count = state(0)
    Text(f"{count.value}")
    Button("+"):
        count.value += 1
```

`Counter` reads `count`, so writing `count` re-runs it. everything below follows from that one rule.

## what runs while composing

the distinction the checks are all about is *when* code runs:

- a composable's body, and the `once` content blocks written in it (`Column:`, `Row:`), run **while composing** — every time the ui is described
- a handler block, a lambda, a nested `def` or an effect block runs **later**, in response to an event

so a read in the body is a dependency of the composition, and a read in a handler is not. a write is the other way round: writing state while composing is an error, and a handler is where writes belong.

the `root` block of `run_app` / `compose_test` starts a composition of its own, so composables may be called there.

## what may be held in state

a `State` notifies its readers when it is *assigned*. a change made *inside* the value it holds notifies nobody, so state may only hold a value that cannot change:

```by
let items = state([1, 2])        # error: mutable-state-value
let items = state((1, 2))        # ok — a tuple cannot change
let todos = state_list([Todo("a")])  # ok — an observable list of frozen records
```

deeply immutable means: the scalars, enum members, a `tuple` or `frozenset` of immutable elements, a `frozen data class` or `NamedTuple` of immutable fields, a type object, a callable, or one of the framework's own observables.

## the diagnostics

| lint                             | what it catches                                           |
| -------------------------------- | --------------------------------------------------------- |
| `mutable-state-value`            | a value that can change, put in state                     |
| `silent-mutation`                | an in-place mutation the composition cannot observe       |
| `state-write-in-composition`     | a state write made while composing                        |
| `unobservable-dependency`        | a composition reading something nothing observes          |
| `composable-outside-composition` | a composable or builder called where nothing is composing |
| `conditional-slot`               | a `state` / `derived` / effect created under a condition  |
| `unstable-parameter`             | a composable parameter the runtime cannot compare         |
| `content-block-control-flow`     | a `return` in a nested content block, which goes nowhere  |

the first five are errors, the last three warnings. all are basedpython-only: under the `ty-compatible` preset they are off.

### unobservable-dependency

a composition may only depend on what it can observe. reading a mutable parameter, global or captured local while composing means the ui goes stale as soon as anything changes it — from another module, a `.py` caller, a callback:

```by
@composable
def Names(items: list[str]):
    Text(str(len(items)))    # error: unobservable-dependency
```

hold it in state (`state_list`, `state_dict`), pass an immutable value (a `tuple`, a `frozen data class`), or read it only in a handler. a name the composition binds itself is this run's own value and is never reported, whatever its type.

### unstable-parameter

a composable is skipped on recomposition only when every argument is *stable* and compares equal to the last one. a `list`, `dict`, `set` or non-frozen class cannot be compared, so the composable re-runs on every recomposition of its parent:

```by
@composable
def TodoList(items: list[int]): ...       # warning: unstable-parameter

@composable
def Skippable(items: tuple[int, ...]): ...  # ok
```

this is about skipping alone. a read-only view (`list[out int]`) *is* stable — the runtime compares it structurally — but reading one while composing is still an `unobservable-dependency`, because the view restricts this reader and not the other holders of the list. it is not the spelling to reach for.

### conditional-slot

a slot lives as long as its enclosing composition scope and is identified by its call site, so one created under a condition is disposed — its state lost, its effect cancelled — as soon as the condition stops holding:

```by
@composable
def Profile(show: bool):
    if show:
        let clicks = state(0)   # warning: conditional-slot
```

state that should outlive a condition belongs above it. a `finally` body is not a condition: it runs however the `try` exited.

## writing a component library

two decorators mark what the checker treats specially, and both are resolved by their definition, so an alias or a re-export works:

- `@composable` — the function's body is a composition scope
- `@builder` — the function emits into the composition being built, so it can only be called while composing

both are defined in `basedpython_ui.runtime` and re-exported by `basedpython_ui`:

```by
from basedpython_ui import builder, Text

@builder
def Badge(text: str):
    Text(f"[{text}]")
```

a builder is not a scope: it emits into the composable that called it and re-runs with it, so it never appears as a recomposition cause — not in the runtime's trace, and not in an `invalidates` hint. what it reads while composing, its caller reads

a helper that emits nothing needs neither, and stays callable from anywhere — including a helper that lives beside the builders in `basedpython_ui.widgets`.

## a `context` parameter with a content block

a [`context` parameter](../features/context-parameters.md) is filled by keyword, so nothing may follow it that a positional argument could land on. the callback a [trailing block](../features/trailing-lambdas.md) fills is the exception, because the call passes it by keyword — but only when it carries the `once` or `local` modifier that marks it a borrowed callback:

```by
def Card(title: str, context theme: str, once content: () -> None):
    content()

Card("x"):
    pass
```

a plain callable parameter after a `context` parameter is rejected: `Card("x", handler)` would bind `handler` to `theme`. write it keyword-only (after a bare `*`) if it must follow.

## in the editor

four inlay hints show what the checker knows, each toggleable through `ty.inlayHints`:

- `inferredReads` — the observables a composable reads, on its header
- `derivedDependencies` — what a `derived(...)` computation depends on
- `parameterStability` — `unstable` before a parameter the runtime cannot compare
- `inferredInvalidations` — what a state write made after composing invalidates, at the end of the statement — or the lambda — that writes

```by
@composable
def Counter(step: int = 1)⟨ reads count, doubled⟩:
    let count = state(0)
    let unread = state(0)
    let doubled = derived(lambda: count.value * 2)⟨ depends on count⟩
    Text(f"{count.value} {doubled.value}")
    Button("+"):
        count.value += step⟨ invalidates Counter, doubled⟩
    Button("reset", on_click=lambda: count.set(0)⟨ invalidates Counter, doubled⟩)
    Button("skip"):
        unread.set(1)⟨ invalidates nothing⟩
```

the read set is a superset: invalidation at runtime always uses the exact set, so an imprecise hint can never mean a missed re-render. a callee that cannot be followed shows as `…`, and so does an unpacked argument (`Child(*cells)`) a callee reads through

### what `invalidates` names

the invalidation set is static, and a superset in the same way. it names every scope the runtime re-runs for the write, in the order they are declared, the write's own file first:

- the composables whose own composition reads the place — in the body, in the `once` / `local` content blocks written in it, and in the plain functions it calls while composing
- a composable handed the slot as an argument, when it reads its parameter — handed directly, through a plain helper's parameter, through a helper that captures the slot, or across modules — rather than the parent that only forwards it: the runtime skips a forwarding parent whose arguments did not change
- a composable called *with a content block* (`Card(count):`), together with its parent. the runtime runs such a child inline: it re-runs whenever its parent does and is never skipped, and what it reads — its own cells included — subscribes the parent's scope, through as many inline parents as there are. so a write to a slot an inline child reads names the child and the parent, and a write that re-runs a parent names every inline child under it
- the `derived` computations whose lambda reads the place and, through them, whatever reads those; a `remember` counts for the scope that made it
- the `root` of `run_app`, `compose_test` and `Runtime.set_root`

a *slot* is a name bound while composing to what a call returned — `let count = state(0)` — in the body or in a content block written in it: a slot declared under `Column:` is the composable's. a name bound to another place — `let alias = count`, `let cell = model.count` — is followed to that place, binding by binding, whether the alias is made in the body or in the handler that writes it

a write nobody observes says `nothing`, and that is said only of a slot no composition reads. what cannot be followed ends the set with `…`, after whatever the walk did see:

- a module-level slot, which another file may read
- a parameter, which a caller in another file may fill
- a slot of a composable with a callable last parameter, which a caller in another file may call with a content block and so subscribe to it — unless the composable is `private`, which no other file can call
- a callee reached through a `dynamic` value, and an unpacked argument (`Child(*cells)`), which may hand the slot to anything
- a written name that is not a slot: a loop or comprehension target, a value bound in a handler or outside every composition, a subscript — what it holds may have readers anywhere

`reads` and `invalidates` disagree on a forwarding parent, on purpose. `def Forwarding(count)⟨ reads count⟩` lifts every composable callee's reads into its caller — a superset that says what the subtree depends on — while a write to that slot says `invalidates Child` and not `Forwarding`, because the runtime subscribes the child's own scope and skips the parent, whose arguments did not change. only a child called with a content block subscribes its parent, and then both hints name the parent

the runtime's exact answer is its trace: why every scope ran, which `bpd` reads and pycharm shows — see [why did this rerender](https://kotlinisland.github.io/basedpython-ui/guide/why-did-this-rerender/)

## limitations

### the read set is approximate

reads are recovered statically by following calls. a callee reached through a `dynamic` value cannot be followed, nor can what an unpacked argument hands a callee, and the hint says so with `…` rather than claiming the set is complete. the invalidation set is built from the same walk, one file at a time, and marks what it cannot see the same way — see [what `invalidates` names](#what-invalidates-names).

### `silent-mutation` sees only what is written here

it reports the mutations in the file it is checking. a mutation made in another module, or by a `.py` caller, is invisible — which is why `unobservable-dependency` exists: keeping a composition from depending on a mutable value in the first place is the guarantee that holds generally.

### recognized on any search path

unlike the other frameworks here, `basedpython_ui` is recognized wherever it resolves — a first-party package as well as an installed one — because it is developed in place. a first-party package named `basedpython_ui` is treated as the framework.
