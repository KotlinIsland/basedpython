# basedpython-ui: the composition lints

A `@composable` function describes a piece of ui as a function of the observables it reads. Its body
and the `once` content blocks written in it (`Column:`, `Row:`) run *while composing*; a handler
block, a lambda, a nested `def` or an effect block written in it runs *later*, in response to an
event. The lints below are all about that distinction, and about what may be held in state. Each
test that needs the framework installs a mock of it in site-packages with exactly the pieces it
uses.

## `is_deeply_immutable`: what may be held in state

A value is *deeply immutable* when nothing reachable from it can change after it is created. The
predicate is exposed for tests through `ty_extensions._internal`.

### the scalars and their literals

```by
from typing import Literal
from ty_extensions import static_assert
from ty_extensions._internal import is_deeply_immutable

static_assert(is_deeply_immutable(int))
static_assert(is_deeply_immutable(float))
static_assert(is_deeply_immutable(bool))
static_assert(is_deeply_immutable(str))
static_assert(is_deeply_immutable(bytes))
static_assert(is_deeply_immutable(None))
static_assert(is_deeply_immutable(complex))
static_assert(is_deeply_immutable(range))
static_assert(is_deeply_immutable(Literal[1, "a", True]))
```

### a container is only as immutable as what it holds

```by
from ty_extensions import static_assert
from ty_extensions._internal import is_deeply_immutable

static_assert(is_deeply_immutable(tuple[int, str]))
static_assert(is_deeply_immutable(tuple[int, ...]))
static_assert(not is_deeply_immutable(tuple[int, list[int]]))
static_assert(is_deeply_immutable(frozenset[int]))
static_assert(not is_deeply_immutable(frozenset[tuple[list[int]]]))
static_assert(not is_deeply_immutable(list[int]))
static_assert(not is_deeply_immutable(dict[str, int]))
static_assert(not is_deeply_immutable(set[int]))
static_assert(not is_deeply_immutable(bytearray))
```

### enum members and enum instances

A basedpython `enum class` counts too: its unit variants are members, and its payload variants are
frozen dataclasses, checked field by field.

```by
from enum import Enum
from typing import Literal
from ty_extensions import static_assert
from ty_extensions._internal import is_deeply_immutable

class Color(Enum):
    RED = 1
    GREEN = 2

static_assert(is_deeply_immutable(Color))
static_assert(is_deeply_immutable(Literal[Color.RED]))

enum class Shape:
    case Point
    case Circle(radius: float)
    case Polygon(points: list[float])

static_assert(is_deeply_immutable(Shape.Point))
static_assert(is_deeply_immutable(Shape.Circle))
static_assert(not is_deeply_immutable(Shape.Polygon))
```

### a record is immutable when it cannot be written and holds only immutable fields

```by
from dataclasses import dataclass
from typing import NamedTuple
from ty_extensions import static_assert
from ty_extensions._internal import is_deeply_immutable

frozen data class Todo:
    title: str
    done: bool = False

frozen data class Bag:
    items: list[int]

data class Draft:
    title: str

@dataclass(frozen=True)
class Frozen:
    x: int

class Point(NamedTuple):
    x: int
    y: int

class Cell(NamedTuple):
    items: list[int]

class Plain:
    x: int = 0

static_assert(is_deeply_immutable(Todo))
static_assert(not is_deeply_immutable(Bag))
static_assert(not is_deeply_immutable(Draft))
static_assert(is_deeply_immutable(Frozen))
static_assert(is_deeply_immutable(Point))
static_assert(not is_deeply_immutable(Cell))
static_assert(not is_deeply_immutable(Plain))
static_assert(not is_deeply_immutable(object))
```

### type objects, callables and the observables are stable by identity

```toml
[environment]
python = "/.venv"
```

The framework's observables are handles whose mutations notify, so a `StateList` is stable even
though what it lists is a `list` inside.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export State, StateList, StateDict, Derived, Ambient
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
class State[T]:
    value: T

class StateList[T]: ...
class StateDict[K, V]: ...

class Derived[T]:
    value: T

class Ambient[T]:
    current: T
```

```by
from typing import Callable
from ty_extensions import static_assert
from ty_extensions._internal import is_deeply_immutable
from basedpython_ui import State, StateList, StateDict, Derived, Ambient

static_assert(is_deeply_immutable(type[int]))
static_assert(is_deeply_immutable(type))
static_assert(is_deeply_immutable(Callable[[], None]))
static_assert(is_deeply_immutable(State[int]))
static_assert(is_deeply_immutable(StateList[list[int]]))
static_assert(is_deeply_immutable(StateDict[str, int]))
static_assert(is_deeply_immutable(Derived[int]))
static_assert(is_deeply_immutable(Ambient[str]))
```

### unions, gradual types and type variables

A union is immutable when every member is; a gradual type says nothing, so it is; a type variable
answers through its bound, and one without a bound stands for whatever the caller passes, which is
checked where the call is solved.

```by
from typing import Any
from ty_extensions import static_assert
from ty_extensions._internal import is_deeply_immutable

static_assert(is_deeply_immutable(int | str | None))
static_assert(not is_deeply_immutable(int | list[int]))
static_assert(is_deeply_immutable(Any))

def unbounded[T](value: T):
    static_assert(is_deeply_immutable(T))

def bounded[T: int](value: T):
    static_assert(is_deeply_immutable(T))

def bounded_by_a_list[T: list[int]](value: T):
    static_assert(not is_deeply_immutable(T))
```

## `mutable-state-value`: a value held in state must be deeply immutable

A `State` notifies its readers when it is assigned; a change made *inside* the held value notifies
nobody. So the initial value of `state(...)` / `State(...)`, the elements of `state_list(...)` /
`StateList(...)`, the value a `derived` / `remember` lambda computes, and every value written into
an observable afterwards must be deeply immutable. The message names the type that cannot be held.

### what a construction call holds

```toml
[environment]
python = "/.venv"
```

It is read off the call's solved result — `state([1, 2])` holds the `list[int]` its
`State[list[int]]` says it does — and reported at the argument.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export State, StateList, state, state_list, derived, remember
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
from collections.abc import Iterable

class State[T]:
    value: T
    def __init__(self, initial: T) -> None: ...

class StateList[T]:
    def __init__(self, initial: Iterable[T] = ()) -> None: ...

class Derived[T]:
    value: T

def state[T](initial: T) -> State[T]: ...
def state_list[T](initial: Iterable[T] = ()) -> StateList[T]: ...
def derived[T](compute: () -> T) -> Derived[T]: ...
def remember[T](compute: () -> T) -> T: ...
```

```by
from basedpython_ui import State, StateList, state, state_list, derived, remember

frozen data class Todo:
    title: str

data class Draft:
    title: str

def slots():
    let count = state(0)
    let names = state(("a", "b"))
    let todo = state(Todo("a"))
    # error: [mutable-state-value] "`list[int]` cannot be held in state: a change to it cannot be observed; use `state_list`, a `tuple`, or a `frozen data class`"
    let items = state([1, 2])
    # error: [mutable-state-value] "`Draft` cannot be held in state: a change to it cannot be observed; use `state_list`, a `tuple`, or a `frozen data class`"
    let draft = state(Draft("a"))
    # error: [mutable-state-value] "`list[int]` cannot be held in state"
    let named = State([1])
    let todos = state_list([Todo("a")])
    # error: [mutable-state-value] "`list[int]` cannot be held in state"
    let nested = state_list([[1]])
    # error: [mutable-state-value] "`set[int]` cannot be held in state"
    let listed = StateList([{1}])
    let total = derived(lambda: count.value + 1)
    # error: [mutable-state-value] "`list[int]` cannot be held in state"
    let doubled = derived(lambda: [count.value])
    let cached = remember(lambda: (1, 2))
    # error: [mutable-state-value] "`dict[str, int]` cannot be held in state"
    let bag = remember(lambda: {"a": 1})
```

### a value written into an observable afterwards

```toml
[environment]
python = "/.venv"
```

The write is checked the same way, whatever the observable's declared type admits.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export State, StateList, StateDict, Ambient, provide
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
class State[T]:
    value: T
    def set(self, new: T) -> None: ...

class StateList[T]:
    def __setitem__(self, index: int, value: T) -> None: ...
    def append(self, value: T) -> None: ...
    def insert(self, index: int, value: T) -> None: ...

class StateDict[K, V]:
    def __setitem__(self, key: K, value: V) -> None: ...

class Ambient[T]:
    current: T

def provide[T](which: Ambient[T], value: T, once content: () -> None) -> None: ...
```

```by
from basedpython_ui import State, StateList, StateDict, Ambient, provide

frozen data class Todo:
    title: str

def writes(cell: State[object], todos: StateList[object], table: StateDict[str, object], scale: Ambient[object]):
    cell.value = 1
    # error: [mutable-state-value] "`list[int]` cannot be held in state"
    cell.value = [1]
    # error: [mutable-state-value] "`list[int]` cannot be held in state"
    cell.set([1])
    todos.append(Todo("b"))
    # error: [mutable-state-value] "`list[int]` cannot be held in state"
    todos.append([1])
    # error: [mutable-state-value] "`list[int]` cannot be held in state"
    todos.insert(0, [1])
    # error: [mutable-state-value] "`list[int]` cannot be held in state"
    todos[0] = [1]
    # error: [mutable-state-value] "`list[int]` cannot be held in state"
    table["a"] = [1]
    provide(scale, 2.0):
        pass
    # error: [mutable-state-value] "`list[int]` cannot be held in state"
    provide(scale, [1]):
        pass
```

### a generic helper is not blamed for its type variable

```toml
[environment]
python = "/.venv"
```

Inside the helper the held type is a type variable, which stands for whatever the caller passes;
that is checked where the call is solved, not in the helper.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export State
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
class State[T]:
    value: T
    def __init__(self, initial: T) -> None: ...
```

```by
from basedpython_ui import State

def hold[T](value: T) -> State[T]:
    return State(value)

let held = hold([1])
```

## `silent-mutation`: an in-place mutation a composition cannot observe

A composition re-runs when an observable it read is written. A `list` or a plain object is not
observable: mutating it in place changes what the ui should show without telling the runtime. The
check reports a mutating call, an in-place operator, a subscript store or delete on a builtin
mutable container, and an attribute store on an instance of a class that is not frozen — anywhere in
a composable: its body, a content block, a handler block, a lambda or a nested `def`.

### in the composable's body

```toml
[environment]
python = "/.venv"
```

The composable is named in the message, and its header carries a secondary annotation.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
```

```by
from basedpython_ui import composable

data class Draft:
    title: str

def load() -> list[str]:
    return []

def load_draft() -> Draft:
    return Draft("")

@composable
def TodoList():
    var items = load()
    items.append("x")  # snapshot: silent-mutation
    # error: [silent-mutation] "`items[...] = ...` mutates `list[str]` in place"
    items[0] = "y"
    # error: [silent-mutation] "`del items[...]` mutates `list[str]` in place"
    del items[0]
    # error: [silent-mutation] "`items += ...` mutates `list[str]` in place"
    items += ["z"]
    # error: [silent-mutation] "`items.sort(...)` mutates `list[str]` in place"
    items.sort()
    let draft = load_draft()
    # error: [silent-mutation] "`draft.title = ...` mutates `Draft` in place, which `TodoList`'s composition cannot observe; mutate a `StateList` or rebuild an immutable value"
    draft.title = "x"
```

```snapshot
error[silent-mutation]: `items.append(...)` mutates `list[str]` in place, which `TodoList`'s composition cannot observe; mutate a `StateList` or rebuild an immutable value
  --> src/mdtest_snippet.by:15:5
   |
13 | def TodoList():
   |     ---------- `TodoList` composes here
14 |     var items = load()
15 |     items.append("x")  # snapshot: silent-mutation
   |     ^^^^^^^^^^^^^^^^^
```

### in a handler block, a lambda and a nested `def`

```toml
[environment]
python = "/.venv"
```

They mutate the value the composition showed, so they are checked too.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
from .widgets export Button, Column
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Button(label: str, on_click: () -> None) -> None: ...
@builder
def Column(once content: () -> None) -> None: ...
```

```by
from basedpython_ui import composable, Button, Column

def load() -> list[str]:
    return []

@composable
def Handlers():
    let items = load()
    Button("add"):
        # error: [silent-mutation] "`items.append(...)` mutates `list[str]` in place, which `Handlers`'s composition cannot observe"
        items.append("x")
    Column:
        # error: [silent-mutation] "`items.clear(...)` mutates `list[str]` in place"
        items.clear()
    # error: [silent-mutation] "`items.pop(...)` mutates `list[str]` in place"
    Button("drop", on_click=lambda: items.pop())

    def later():
        # error: [silent-mutation] "`items.reverse(...)` mutates `list[str]` in place"
        items.reverse()
```

### a fresh local and an observable may be mutated

```toml
[environment]
python = "/.venv"
```

A container or instance the composition creates itself — bound to a display, a comprehension or a
constructor call — is a fresh local that nothing else holds. A `StateList`'s writes notify.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable, state_list
from .widgets export Button, Column
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
from collections.abc import Iterable

class StateList[T]:
    def append(self, value: T) -> None: ...

def state_list[T](initial: Iterable[T] = ()) -> StateList[T]: ...
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Button(label: str, on_click: () -> None) -> None: ...
@builder
def Column(once content: () -> None) -> None: ...
```

```by
from basedpython_ui import composable, state_list, Button, Column

data class Draft:
    title: str

@composable
def FreshLocals():
    let table: dict[str, int] = {}
    table["a"] = 1
    let squares = [n * n for n in range(3)]
    squares.append(9)
    let made = Draft("a")
    made.title = "b"
    let todos = state_list(["a"])
    Button("add"):
        todos.append("b")
    Column:
        table["b"] = 2
```

### a read-only view is already rejected, and a plain function is not a composition

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
```

```by
from basedpython_ui import composable

data class Draft:
    title: str

@composable
def ReadOnly(items: list[out str]):
    # error: [invalid-argument-type]
    # error: [unobservable-dependency]
    items.append("x")

def helper(items: list[str], draft: Draft):
    items.append("x")
    draft.title = "y"
```

## `state-write-in-composition`: state is written from handlers and effects

Composition is a pure description of the ui for the current state; a write made while composing
invalidates the frame being built, and the runtime raises before applying it. An assignment to a
`State`'s `.value`, `State.set` / `State.update`, and every mutator of a `StateList` / `StateDict`
are reported in a composable's body and in the content blocks written in it. The observable is named
as it was written.

### in the composable's body

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export StateDict, composable, state, state_list, state_dict
from .widgets export Text
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
from collections.abc import Iterable

class State[T]:
    value: T
    def set(self, new: T) -> None: ...
    def update(self, fn: (T) -> T) -> None: ...

class StateList[T]:
    def __setitem__(self, index: int, value: T) -> None: ...
    def append(self, value: T) -> None: ...
    def clear(self) -> None: ...

class StateDict[K, V]:
    def __setitem__(self, key: K, value: V) -> None: ...
    def remove(self, key: K) -> None: ...

def state[T](initial: T) -> State[T]: ...
def state_list[T](initial: Iterable[T] = ()) -> StateList[T]: ...
def state_dict[K, V]() -> StateDict[K, V]: ...
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
```

```by
from basedpython_ui import StateDict, composable, state, state_list, state_dict, Text

@composable
def Counter():
    let count = state(0)
    let todos = state_list([1])
    let table: StateDict[str, int] = state_dict()
    # error: [state-write-in-composition] "`count` is written while `Counter` is composing; move the write into an event handler or an effect"
    count.value = 1
    # error: [state-write-in-composition] "`count` is written while `Counter` is composing"
    count.value += 1
    # error: [state-write-in-composition] "`count` is written while `Counter` is composing"
    count.set(2)
    # error: [state-write-in-composition] "`count` is written while `Counter` is composing"
    count.update(lambda c: c + 1)
    # error: [state-write-in-composition] "`todos` is written while `Counter` is composing"
    todos.append(2)
    # error: [state-write-in-composition] "`todos` is written while `Counter` is composing"
    todos[0] = 3
    # error: [state-write-in-composition] "`todos` is written while `Counter` is composing"
    todos.clear()
    # error: [state-write-in-composition] "`table` is written while `Counter` is composing"
    table["a"] = 1
    # error: [state-write-in-composition] "`table` is written while `Counter` is composing"
    table.remove("a")
    Text(f"{count.value}")
```

### which scopes run while composing

```toml
[environment]
python = "/.venv"
```

A content block runs while composing, and so does the `local` block of a keyed `each`; a handler
block, a lambda, a nested `def` and an effect block run later, which is where writes belong.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable, state, state_list, launched_effect
from .widgets export Button, Column
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
from collections.abc import Iterable

class Job: ...

class State[T]:
    value: T
    def set(self, new: T) -> None: ...

class StateList[T]:
    def each(self, key: (T) -> object, local content: (T) -> None) -> None: ...

def state[T](initial: T) -> State[T]: ...
def state_list[T](initial: Iterable[T] = ()) -> StateList[T]: ...
def launched_effect(key: object, block: (Job) -> None) -> None: ...
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Button(label: str, on_click: () -> None) -> None: ...
@builder
def Column(once content: () -> None) -> None: ...
```

```by
from basedpython_ui import composable, state, state_list, launched_effect, Button, Column

@composable
def Scopes():
    let count = state(0)
    let todos = state_list([1])
    Column:
        # error: [state-write-in-composition] "`count` is written while `Scopes` is composing"
        count.value = 3
    todos.each(key=lambda n: n):
        # error: [state-write-in-composition] "`count` is written while `Scopes` is composing"
        count.value = it
    Button("+"):
        count.value += 1
    Button("reset", on_click=lambda: count.set(0))
    launched_effect(count.value):
        count.value = 5

    def later():
        count.value = 0
```

## `conditional-slot`: a slot is created and disposed with its condition

A slot — `state`, `state_list`, `state_dict`, `derived`, `remember` and the effects — lives as long
as its composition scope and is identified by its call site. Created under a condition, it is
created when the condition first holds and disposed as soon as it stops holding: its state is lost
and its effect cancelled. The runtime handles that correctly; the warning makes the lifetime
visible.

### every conditional construct counts

```toml
[environment]
python = "/.venv"
```

So do a comprehension and a conditional expression.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable, state, derived, remember, launched_effect, disposable_effect, side_effect
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
class Job: ...
class DisposeScope: ...

class State[T]:
    value: T

class Derived[T]:
    value: T

def state[T](initial: T) -> State[T]: ...
def derived[T](compute: () -> T) -> Derived[T]: ...
def remember[T](compute: () -> T) -> T: ...
def launched_effect(key: object, block: (Job) -> None) -> None: ...
def disposable_effect(key: object, block: (DisposeScope) -> None) -> None: ...
def side_effect(block: () -> None) -> None: ...
def composable[F](fn: F) -> F: ...
```

```by
from basedpython_ui import composable, state, derived, remember, launched_effect, disposable_effect, side_effect

@composable
def Profile(show: bool, ids: tuple[int, ...]):
    let clicks = state(0)
    if show:
        # error: [conditional-slot] "`state()` under a condition: it will be created and disposed as the condition changes"
        let extra = state(0)
    for id in ids:
        # error: [conditional-slot] "`derived()` under a condition: it will be created and disposed as the condition changes"
        per = derived(lambda: id)
    while show:
        # error: [conditional-slot] "`remember()` under a condition"
        remember(lambda: 1)
        break
    try:
        # error: [conditional-slot] "`side_effect()` under a condition"
        side_effect:
            pass
    except ValueError:
        pass
    match show:
        case True:
            # error: [conditional-slot] "`launched_effect()` under a condition"
            launched_effect(1):
                pass
        case _:
            pass
    # error: [conditional-slot] "`state()` under a condition"
    let listed = [state(i) for i in ids]
    # error: [conditional-slot] "`state()` under a condition"
    let picked = state(1) if show else None
    disposable_effect(1):
        pass
```

### a `finally` body is not a condition

Every other part of a `try` depends on how the body exited, so a slot in one lives as long as that
outcome. A `finally` body runs whatever happened above it, so a slot written there is created
exactly as often as the statement is reached.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable, state
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
class State[T]:
    value: T

def state[T](initial: T) -> State[T]: ...
def composable[F](fn: F) -> F: ...
```

```by
from basedpython_ui import composable, state

def risky() -> None: ...

@composable
def Cleanup():
    try:
        risky()
        # error: [conditional-slot] "`state()` under a condition"
        let started = state(0)
    except Exception:
        # error: [conditional-slot] "`state()` under a condition"
        let failed = state(0)
    else:
        # error: [conditional-slot] "`state()` under a condition"
        let succeeded = state(0)
    finally:
        let always = state(0)
```

### a content block runs with its composable; a handler block does not

```toml
[environment]
python = "/.venv"
```

A slot in a content block is fine — unless the block itself sits under a condition. A block that is
not `once` (a handler) may run any number of times, and a slot created there has no scope to live
in.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable, state
from .widgets export Button, Column
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
class State[T]:
    value: T

def state[T](initial: T) -> State[T]: ...
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Button(label: str, on_click: () -> None) -> None: ...
@builder
def Column(once content: () -> None) -> None: ...
```

```by
from basedpython_ui import composable, state, Button, Column

@composable
def Blocks(show: bool):
    Column:
        let inner = state(0)
        if show:
            # error: [conditional-slot] "`state()` under a condition"
            let cond = state(0)
    if show:
        Column:
            # error: [conditional-slot] "`state()` under a condition"
            let nested = state(0)
    Button("x"):
        # error: [conditional-slot] "`state()` under a condition"
        let handler_state = state(0)
```

## `content-block-control-flow`: a `return` in a nested content block goes nowhere

A `once` block's `return` leaves the scope the block is written in — but only that one. When that
scope is itself a block, the `return` leaves the inner block and stops: the enclosing function keeps
running and the value is discarded. This is a property of the language, so it needs no framework:
any `once` callee will do. (A `break` or `continue` in a block is already rejected as `break`
outside loop.)

```by
def Column(once content: () -> None):
    content()

def Row(once content: () -> None):
    content()

def outer() -> int:
    Column:
        Row:
            # error: [content-block-control-flow] "`return` inside a nested content block leaves only the block; it cannot leave `outer`"
            return 1
        return 2
    return 0
```

## `unstable-parameter`: an unstable argument is never skipped

A composable is skipped on recomposition only when every argument is stable and equal to the last
one. A parameter whose declared type is not deeply immutable — and not a read-only view of immutable
elements — disables skipping for the whole scope. This is a warning about skipping alone: whether
the composition may *read* such a parameter is the question `unobservable-dependency` asks, so the
suggested spellings never include a read-only view.

### the message suggests the stable spellings for the shape at hand

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
```

```by
from basedpython_ui import composable

data class Draft:
    title: str

@composable
def TodoList(
    # error: [unstable-parameter] "`items: list[int]` is unstable, so `TodoList` is never skipped; prefer `tuple[int, ...]`, `state_list`, or a `frozen data class`"
    items: list[int],
    # error: [unstable-parameter] "`tags: set[str]` is unstable, so `TodoList` is never skipped; prefer `frozenset[str]` or `state_list`"
    tags: set[str],
    # error: [unstable-parameter] "`table: dict[str, int]` is unstable, so `TodoList` is never skipped; prefer `state_dict` or a `frozen data class`"
    table: dict[str, int],
    # error: [unstable-parameter] "`draft: Draft` is unstable, so `TodoList` is never skipped; prefer a `frozen data class` or an observable"
    draft: Draft,
    # error: [unstable-parameter] "`theme: Draft` is unstable, so `TodoList` is never skipped; prefer a `frozen data class` or an observable"
    context theme: Draft,
    once content: () -> None,
):
    content()
```

### a use-site modifier does not change what a parameter is

`final list[out int]` is the read-only view inside it, and `final list[str]` is a plain list — a
restriction written at the use site says nothing about stability, and the message names the shape it
finds underneath.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
```

```by
from basedpython_ui import composable

@composable
def Viewed(items: final list[out int]): ...

@composable
# error: [unstable-parameter] "`items: final list[str]` is unstable, so `Held` is never skipped; prefer `tuple[str, ...]`, `state_list`, or a `frozen data class`"
def Held(items: final list[str]): ...
```

### immutable values, observables, callables and read-only views are stable

```toml
[environment]
python = "/.venv"
```

So is an unannotated parameter, about which nothing is known; and a plain function's parameters are
not looked at.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export StateList, composable
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
class StateList[T]: ...

def composable[F](fn: F) -> F: ...
```

```by
from basedpython_ui import StateList, composable

frozen data class Todo:
    title: str

@composable
def Skippable(
    count: int,
    name: str | None,
    todo: Todo,
    ids: tuple[int, ...],
    todos: StateList[Todo],
    view: list[out int],
    on_click: () -> None,
    anything,
    once content: () -> None,
):
    content()

def helper(items: list[int]): ...
```

## `composable-outside-composition`: a composable is called while composing

A composable opens a scope in the composition being built and a builder emits into it; neither has
anything to build into outside of one.

### an ordinary helper of the widgets module is not a builder

The `basedpython_ui.widgets` module is free to hold functions that emit nothing — a helper that
computes a default, say. Being declared there is not what makes a function a builder; the
framework's `@builder` decorator is, exactly as `@composable` is what makes a composition scope. An
undecorated helper stays callable from anywhere.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...

def default_padding() -> int: ...
```

```by
from basedpython_ui.widgets import Text, default_padding

let pad = default_padding()

def measure() -> int:
    return default_padding()

# error: [composable-outside-composition] "`Text` is a builder and can only be called while composing"
Text("x")
```

### a plain function and the module are not compositions

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
from .widgets export Text
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
```

```by
from basedpython_ui import composable, Text

@composable
def Counter(): ...

@composable
def Card(once content: () -> None):
    content()

def helper():
    # error: [composable-outside-composition] "`Counter` is a composable and can only be called while composing"
    Counter()
    # error: [composable-outside-composition] "`Text` is a builder and can only be called while composing"
    Text("x")
    # error: [composable-outside-composition] "`Card` is a composable and can only be called while composing"
    Card:
        pass

# error: [composable-outside-composition] "`Counter` is a composable and can only be called while composing"
Counter()
```

### inside a composition the calls are what composition is made of

```toml
[environment]
python = "/.venv"
```

A composable's body, its content blocks and a keyed `each` are compositions. A handler block, a
lambda and a nested `def` run after composition, so a call from one of those is reported.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable, keyed
from .widgets export Text, Button, Column
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
from collections.abc import Iterable

class Keyed[T]:
    def each(self, key: (T) -> object, local content: (T) -> None) -> None: ...

def keyed[T](items: Iterable[T]) -> Keyed[T]: ...
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
@builder
def Button(label: str, on_click: () -> None) -> None: ...
@builder
def Column(once content: () -> None) -> None: ...
```

```by
from basedpython_ui import composable, keyed, Text, Button, Column

@composable
def Counter(): ...

@composable
def Card(once content: () -> None):
    content()

@composable
def App():
    Counter()
    Text("x")
    Column:
        Counter()
    Card:
        Counter()
    keyed(("a", "b")).each(key=lambda s: s):
        Text(it)
    Button("x"):
        # error: [composable-outside-composition] "`Counter` is a composable and can only be called while composing"
        Counter()
    # error: [composable-outside-composition] "`Counter` is a composable and can only be called while composing"
    Button("y", on_click=lambda: Counter())

    def later():
        # error: [composable-outside-composition] "`Text` is a builder and can only be called while composing"
        Text("z")
```

### the root of an app, a test or a runtime is where a composition starts

```toml
[environment]
python = "/.venv"
```

The `root` block of `run_app` / `compose_test` is a composition of its own. So is whatever is handed
to the runtime's own `Runtime.set_root` — a lambda or a function — which the two wrap and which a
test or a benchmark drives directly.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export Runtime, composable
from .widgets export Text
from .app export run_app, compose_test
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
class Runtime:
    def set_root(self, root: () -> None) -> None: ...

def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/app.byi`:

```byi
class TestComposition: ...

def run_app(title: str, root: () -> None) -> None: ...
def compose_test(root: () -> None) -> TestComposition: ...
```

```by
from basedpython_ui import Runtime, composable, Text, run_app, compose_test

@composable
def Counter(step: int = 1): ...

def main():
    run_app("app"):
        Counter()
    let t = compose_test:
        Counter(step=2)

def bench(rt: Runtime):
    rt.set_root(lambda: Counter(2))
    Runtime().set_root(lambda: Text("x"))

    def root():
        Counter()

    rt.set_root(root)
    rt.set_root(root=lambda: Counter(3))

def elsewhere(rt: Runtime):
    # error: [composable-outside-composition] "`Counter` is a composable and can only be called while composing"
    let make = lambda: Counter()
    rt.set_root(make)
```

## `unobservable-dependency`: a composition reads only what it can observe

A mutation of non-observable data is never a trigger: an immutable value cannot change, an
observable notifies its readers when it does, and a `list`, a `dict` or a plain object changes
without telling anyone. So a composition may only depend on immutable or observable values — then it
does not matter where a write happens. The check reports a load, while composing, of a name the
composition did not bind itself: a parameter of the composable, a module global, or a local captured
from an enclosing function, whose type is neither deeply immutable nor an observable. What runs
while composing is the composable's body, the `once` content blocks and `local` blocks written in
it, and the lambda given to `derived` / `remember`; a handler block, any other lambda, a nested
`def` or an effect block runs later, and a read there is not a dependency of the composition.

### a parameter read in the body

```toml
[environment]
python = "/.venv"
```

The message names the parameter with its type, and the composable's header carries a secondary
annotation. A read in a content block written in the body is a read of the composition too.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
from .widgets export Text, Column
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
@builder
def Column(once content: () -> None) -> None: ...
```

```by
from basedpython_ui import composable, Text, Column

@composable
def Names(
    # error: [unstable-parameter]
    items: list[str],
):
    Text(str(len(items)))  # snapshot: unobservable-dependency
    Column:
        # error: [unobservable-dependency] "`items: list[str]` is read while `Names` composes"
        Text(items[0])
```

```snapshot
error[unobservable-dependency]: `items: list[str]` is read while `Names` composes, but nothing observes a change to it; hold it in state (`state_list`), pass an immutable value (`tuple[str, ...]`, a `frozen data class`), or read it only in a handler
 --> src/mdtest_snippet.by:8:18
  |
4 |   def Names(
  |  _____-
5 | |     # error: [unstable-parameter]
6 | |     items: list[str],
7 | | ):
  | |_- `Names` composes here
8 |       Text(str(len(items)))  # snapshot: unobservable-dependency
  |                    ^^^^^
```

### the message suggests the observable spellings for the shape at hand

```toml
[environment]
python = "/.venv"
```

The state constructor that holds a value of the parameter's shape, and an immutable spelling of it;
a plain object has neither, and should be a frozen record or an observable instead.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
from .widgets export Text
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
```

```by
from basedpython_ui import composable, Text

data class Draft:
    title: str

@composable
def Shapes(
    # error: [unstable-parameter]
    tags: set[str],
    # error: [unstable-parameter]
    table: dict[str, int],
    # error: [unstable-parameter]
    draft: Draft,
):
    # error: [unobservable-dependency] "`tags: set[str]` is read while `Shapes` composes, but nothing observes a change to it; hold it in state (`state_list`), pass an immutable value (`frozenset[str]`), or read it only in a handler"
    Text(str(len(tags)))
    # error: [unobservable-dependency] "`table: dict[str, int]` is read while `Shapes` composes, but nothing observes a change to it; hold it in state (`state_dict`), pass an immutable value (a `frozen data class`), or read it only in a handler"
    Text(str(len(table)))
    # error: [unobservable-dependency] "`draft: Draft` is read while `Shapes` composes, but nothing observes a change to it; pass a `frozen data class` or an observable, or read it only in a handler"
    Text(draft.title)
```

### a parameter read only in a handler is not a dependency

```toml
[environment]
python = "/.venv"
```

The handler runs after composition, so the composition never depended on the value. What remains is
the `unstable-parameter` warning: the composable is never skipped.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
from .widgets export Button
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Button(label: str, on_click: () -> None) -> None: ...
```

```by
from basedpython_ui import composable, Button

@composable
def Handler(
    # error: [unstable-parameter] "`items: list[str]` is unstable, so `Handler` is never skipped; prefer `tuple[str, ...]`, `state_list`, or a `frozen data class`"
    items: list[str],
):
    Button("count"):
        print(len(items))
    Button("last", on_click=lambda: print(items[-1]))
```

### a python file is told the spelling its own syntax has

The checks apply to a `.py` file that uses the framework, so the suggestions have to be spellings a
python file can actually write.

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.pyi`:

```pyi
from .runtime import composable as composable
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.pyi`:

```pyi
def composable[F](fn: F) -> F: ...
```

```py
from basedpython_ui import composable

@composable
# error: [unstable-parameter] "prefer `tuple[str, ...]`, `state_list`, or a `@dataclass(frozen=True)`"
def Names(items: list[str]):
    # error: [unobservable-dependency] "hold it in state (`state_list`), pass an immutable value (`tuple[str, ...]`, a `@dataclass(frozen=True)`), or read it only in a handler"
    print(len(items))
```

### a mutable global read in the body

```toml
[environment]
python = "/.venv"
```

A global is named with its type; an immutable global is fine.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
from .widgets export Text
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
```

```by
from basedpython_ui import composable, Text

frozen data class Todo:
    title: str

let TODOS: list[Todo] = []
let TITLES: tuple[str, ...] = ("a", "b")

@composable
def Names():
    # error: [unobservable-dependency] "`TODOS` (`list[Todo]`) is read while `Names` composes, but nothing observes a change to it; hold it in state (`state_list`), make it immutable, or read it only in a handler"
    Text(str(len(TODOS)))
    Text(TITLES[0])
```

### a local captured from an enclosing function

```toml
[environment]
python = "/.venv"
```

A composable defined inside a function reads that function's locals as a closure; they are as
invisible to it as a global.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
from .widgets export Text
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
```

```by
from basedpython_ui import composable, Text

def make():
    let items: list[str] = []
    let names: tuple[str, ...] = ("a",)

    @composable
    def Names():
        # error: [unobservable-dependency] "`items` (`list[str]`) is read while `Names` composes, but nothing observes a change to it; hold it in state (`state_list`), make it immutable, or read it only in a handler"
        Text(str(len(items)))
        Text(names[0])
```

### a read-only view restricts only this reader

```toml
[environment]
python = "/.venv"
```

A `list[out str]` parameter cannot be written through, but the list behind it can be written by
whoever else holds it — so it is reported like a plain `list`.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
from .widgets export Text
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
```

```by
from basedpython_ui import composable, Text

@composable
def View(items: list[out str]):
    # error: [unobservable-dependency] "`items: list[out str]` is read while `View` composes, but nothing observes a change to it; hold it in state (`state_list`), pass an immutable value (`tuple[str, ...]`, a `frozen data class`), or read it only in a handler"
    Text(str(len(items)))
```

### immutable values and observables are what a composition may depend on

```toml
[environment]
python = "/.venv"
```

A frozen record, a tuple, a `StateList`, a `State`, an `Ambient`'s `current`, a frozen `context`
parameter and a callable are all fine to read while composing.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export State, StateList, Ambient, composable, ambient
from .widgets export Text, Button
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
class State[T]:
    value: T

class StateList[T]:
    def __len__(self) -> int: ...

class Ambient[T]:
    current: T

def ambient[T](default: T) -> Ambient[T]: ...
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
@builder
def Button(label: str, on_click: () -> None) -> None: ...
```

```by
from basedpython_ui import State, StateList, Ambient, composable, ambient, Text, Button

frozen data class Theme:
    name: str

let density = ambient(1.0)

@composable
def Seen(
    todo: Theme,
    ids: tuple[int, ...],
    todos: StateList[str],
    count: State[int],
    on_click: () -> None,
    context theme: Theme,
):
    Text(todo.name)
    Text(str(ids[0]))
    Text(str(len(todos)))
    Text(str(count.value))
    Text(str(density.current))
    Text(theme.name)
    Button("x", on_click=on_click)
```

### a local the composition creates is its own value

```toml
[environment]
python = "/.venv"
```

A name bound in the body — whatever its origin — is this run's value, and so is one bound in a
content block written in the body, a `for` target or a comprehension variable. Reading it is not
depending on anything outside the composition.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
from .widgets export Text, Column
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
@builder
def Column(once content: () -> None) -> None: ...
```

```by
from basedpython_ui import composable, Text, Column

def load() -> list[str]:
    return []

@composable
def Own():
    let items = load()
    let table: dict[str, int] = {}
    Text(str(len(items)))
    Column:
        Text(str(len(table)))
        let inner = load()
        Text(str(len(inner)))
    for item in items:
        Text(item)
    Text(str([len(name) for name in items]))
```

### the lambda given to `derived` / `remember` reads for the composition

```toml
[environment]
python = "/.venv"
```

What the computation reads is what it depends on, so a mutable parameter read there is reported; any
other lambda runs later.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable, state, derived, remember
from .widgets export Text, Button
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
class State[T]:
    value: T

class Derived[T]:
    value: T

def state[T](initial: T) -> State[T]: ...
def derived[T](compute: () -> T) -> Derived[T]: ...
def remember[T](compute: () -> T) -> T: ...
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
@builder
def Button(label: str, on_click: () -> None) -> None: ...
```

```by
from basedpython_ui import composable, state, derived, remember, Text, Button

@composable
def Computed(
    # error: [unstable-parameter]
    items: list[str],
):
    let count = state(0)
    # error: [unobservable-dependency] "`items: list[str]` is read while `Computed` composes"
    let total = derived(lambda: len(items) + count.value)
    # error: [unobservable-dependency] "`items: list[str]` is read while `Computed` composes"
    let first = remember(lambda: items[0])
    Text(str(total.value))
    Text(first)
    Button("log", on_click=lambda: print(len(items)))
```

### a helper in another module cannot make the composition stale

```toml
[environment]
python = "/.venv"
```

The write-side check cannot see a mutation made by a callee in another module. It does not need to:
the composition is reported where it reads the list, and the helper — no composition at all — is
not.

`/.venv/<path-to-site-packages>/basedpython_ui/__init__.byi`:

```byi
from .runtime export composable
from .widgets export Text, Button
```

`/.venv/<path-to-site-packages>/basedpython_ui/runtime.byi`:

```byi
def composable[F](fn: F) -> F: ...
def builder[F](fn: F) -> F: ...
```

`/.venv/<path-to-site-packages>/basedpython_ui/widgets.byi`:

```byi
from .runtime import builder

@builder
def Text(text: str) -> None: ...
@builder
def Button(label: str, on_click: () -> None) -> None: ...
```

`helpers.by`:

```by
def add_item(items: list[str], item: str):
    items.append(item)
```

```by
from basedpython_ui import composable, Text, Button
from helpers import add_item

@composable
def Names(
    # error: [unstable-parameter]
    items: list[str],
):
    # error: [unobservable-dependency] "`items: list[str]` is read while `Names` composes, but nothing observes a change to it; hold it in state (`state_list`), pass an immutable value (`tuple[str, ...]`, a `frozen data class`), or read it only in a handler"
    Text(str(len(items)))
    Button("add"):
        add_item(items, "x")
```
