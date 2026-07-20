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
