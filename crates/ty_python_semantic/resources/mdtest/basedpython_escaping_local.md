# basedpython: `local` parameters cannot escape

A `local` parameter is borrowed for the duration of the call. Handing its value to something that
outlives the call — returning it, storing it on a parameter-rooted object, binding it to a `global`
/ `nonlocal` name, or passing it on to a parameter that is not itself `local` — is rejected with
`escaping-local`. The value may otherwise be used freely.

```toml
[environment]
python-version = "3.12"
```

## a local may be used freely within the call

```by
def use(local fn: () -> None):
    fn()  # called
    forward(fn)  # handed on to another `local` parameter — allowed
    kept = fn  # bound to an ordinary local
    kept()

def forward(local cb: () -> None):
    cb()
```

## passing a local to a non-`local` parameter escapes

A `local` may only be handed on to a parameter that is itself `local` — a plain parameter could
retain it past the call.

```by
def sink(cb: () -> None):
    cb()

def f(local fn: () -> None):
    # error: [escaping-local] "local `fn` cannot escape the call: it is passed as a non-`local` argument"
    sink(fn)
```

## keyword and variadic parameters are checked too

```by
def sink(*cbs: object):
    print(cbs)

def kw(cb: () -> None):
    cb()

def f(local fn: () -> None):
    sink(fn)  # error: [escaping-local]
    kw(cb=fn)  # error: [escaping-local]
```

## passing a local to a `local` parameter is fine

```by
def forward(local cb: () -> None):
    cb()

def f(local fn: () -> None):
    forward(fn)
```

## an opaque callee is left alone

When the callee's signature cannot be resolved, its parameter's declaration cannot be inspected, so
the call is not flagged.

```by
# error: [unresolved-import]
from nowhere import sink

def f(local fn: () -> None):
    sink(fn)
```

## returning a local escapes

```by
def f(local fn: () -> None) -> object:
    return fn  # error: [escaping-local] "local `fn` cannot escape the call: it is returned from the call"
```

## returning a local held in a container escapes

```by
def f(local fn: () -> None) -> object:
    return [fn]  # error: [escaping-local]
```

## returning a local through a ternary escapes

Both arms of a conditional expression hand the value straight to the caller.

```by
def f(local fn: () -> None, c: bool) -> object:
    return fn if c else None  # error: [escaping-local]
```

## returning a local through a boolean escapes

```by
def f(local fn: () -> None, fallback: object) -> object:
    return fn or fallback  # error: [escaping-local]
```

## augmented-assigning a local into a parameter's attribute escapes

`self.items += [fn]` mutates a parameter-rooted container in place, so the local reaches storage
that outlives the call.

```by
class Registry:
    items: list[object]

    def add(self, local fn: () -> None):
        self.items += [fn]  # error: [escaping-local]
```

## storing a local on a global's attribute escapes

A store into an attribute of a `global` name outlives the call just as a bare store does.

```by
class Box:
    fn: object = None

_box: Box = Box()

def f(local fn: () -> None):
    global _box
    _box.fn = fn  # error: [escaping-local]
```

## storing a local on `self` escapes

```by
class Registry:
    def register(self, local fn: () -> None):
        self.fn = fn  # error: [escaping-local]
```

## storing a local into a parameter's item escapes

```by
def f(sink: list[object], local fn: () -> None):
    sink[0] = fn  # error: [escaping-local]
```

## binding a local to a global escapes

```by
_saved: object = None

def f(local fn: () -> None):
    global _saved
    _saved = fn  # error: [escaping-local] "local `fn` cannot escape the call: it is stored where it outlives the call"
```

## assigning a local to a plain local is fine

The value stays inside the call, so there is nothing to flag.

```by
def f(local fn: () -> None):
    kept = fn
    kept()
```

## a non-local parameter may escape

Only `local` parameters are constrained.

```by
def f(fn: () -> None) -> object:
    return fn
```

## a callback's parameter may be declared `local`

A callable type can mark its own parameters `local`. That constrains the *implementation* of the
callback rather than its callers: the value handed in is local to the call, so the body filling that
callable may not let it escape. The flagship case is a trailing lambda block, whose implicit `it`
binds the marked position.

```by
class Resource: ...

def sink(r: Resource): ...

def f(fn: (local Resource) -> None):
    fn(Resource())

f:
    # error: [escaping-local] "local `it` cannot escape the call: it is passed as a non-`local` argument"
    sink(it)
```

## a receiver callback's `local` argument is still `it`

A block binds its callback's [receiver](basedpython_implicit_receiver.md) implicitly, as `self`, so
the parameter *after* the receiver is the one `it` binds — and the one whose `local` constrains the
block.

```by
class Resource: ...

def sink(r: Resource): ...

def f(fn: str.(local Resource) -> None):
    fn("a", Resource())

f:
    # error: [escaping-local] "local `it` cannot escape the call: it is passed as a non-`local` argument"
    sink(it)
```

## a borrowed `it` may be used freely within the block

```by
class Resource:
    def read(self) -> str:
        return ""

def borrow(local r: Resource): ...

def f(fn: (local Resource) -> None):
    fn(Resource())

f:
    it.read()  # a member call keeps it inside the block
    borrow(it)  # re-lent to another borrow
```

## a borrowed `it` may not be written back to an enclosing binding

A trailing lambda block's assignments write *through* to an enclosing binding (the lowering inserts
the matching `global` / `nonlocal`), so binding a borrow to one lets it outlive the call.

```by
class Resource: ...

def f(fn: (local Resource) -> None):
    fn(Resource())

var kept: Resource | None = None

f:
    # error: [escaping-local] "local `it` cannot escape the call: it is stored where it outlives the call"
    kept = it
```

## a block-local binding is fine

A name the block alone binds dies with the block, so it does not carry the borrow out.

```by
class Resource: ...

def f(fn: (local Resource) -> None):
    fn(Resource())

f:
    tmp = it
    print(tmp is None)
```

## a `once` block's fresh binding still escapes

A `once` block runs exactly once, so even a name only it binds survives the block — the lowering
makes it an enclosing local. A borrow bound to one therefore does escape.

```by
class Resource: ...

def f(once fn: (local Resource) -> None):
    fn(Resource())

f:
    # error: [escaping-local] "local `it` cannot escape the call: it is stored where it outlives the call"
    kept = it
```

## returning a borrowed `it` escapes

```by
class Resource: ...

def f(once fn: (local Resource) -> None) -> Resource:
    fn(Resource())
    return Resource()

f:
    # error: [escaping-local] "local `it` cannot escape the call: it is returned from the call"
    return it
```

## the borrowed parameter may be named

Naming the callback's parameter documents it, and is also the spelling that reaches a type a bare
modifier cannot precede — the modifier is only read when a name follows it.

```by
class Resource: ...

def sink(r: Resource): ...

def f(fn: (local resource: Resource) -> None):
    fn(Resource())

f:
    sink(it)  # error: [escaping-local]
```

## an unmarked callback parameter leaves the block unconstrained

```by
class Resource: ...

def sink(r: Resource): ...

def f(fn: (Resource) -> None):
    fn(Resource())

f:
    sink(it)
```

## an opaque callee leaves the block unconstrained

When the callee's callback shape cannot be inspected, there is no declaration to read, so the block
is left alone — as everywhere else in the borrow analysis.

```by
def sink(r: object): ...

def f(fn): ...

f:
    sink(it)
```

## a builtin that cannot retain its argument takes a borrow

The escape rule is that a `local` handed to a non-`local` parameter escapes, since that callee might
keep it. The builtins that provably cannot — they return a fresh scalar and hand no part of the
argument back — say so in their own signatures, so ordinary reads of a borrow are not reported.

```by
def f(local xs: list[int]) -> None:
    print(len(xs))
    print(sum(xs))
    print(any(xs))
    print(all(xs))
    print(repr(xs))
    print(hash(len(xs)))
    print(isinstance(xs, list))
```

Anything that could keep it still escapes, including a callee of the user's own.

```by
_registry: list[object] = []

def keeps(x: object) -> None:
    _registry.append(x)

def g(local xs: list[int]) -> None:
    # error: [escaping-local]
    keeps(xs)
    # error: [escaping-local]
    _registry.append(xs)
```
