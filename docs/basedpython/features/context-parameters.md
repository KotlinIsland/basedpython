# context parameters

a `context` parameter is filled implicitly at call sites from the `context`
declarations in scope, threading ambient values (a config, a logger, a
connection) through call chains without explicit forwarding

```by
def f(a: int, context b: str): ...

f(1)                # error: no context value found for parameter `b`
context s1 = "asdf"
f(2)                # ok — `s1` is passed implicitly
context s2 = "fdsa"
f(3)                # error: ambiguous — `s1` and `s2` both match
```

## surface syntax

`context` marks a parameter, written ahead of its name:

```by
def render(template: str, context theme: Theme, context log: Logger): ...
```

and declares a value, written ahead of an assignment. the declaration may
carry an annotation; without one the variable is typed by its value:

```by
context theme = Theme("dark")
context log: Logger = make_logger()
```

`context` is a prefix on a declaration rather than a form of its own, so it
composes with the rest of the [declaration modifiers](modifiers.md) in either
order. the other keywords decide what the declaration is — `let` makes it
`Final`, `private` hides it — and `context` only adds the candidacy:

```by
context let theme: Theme = Theme("dark")
private context var attempts = 0
```

it is not a modifier on a `def` or a `class`, which declare no variable for a
call site to read

## resolution

resolution is by **assignability, not by name**: a declaration is a candidate
for a parameter when its type is assignable to the parameter's declared type.
the rules:

- the **innermost scope** with at least one candidate wins; a declaration in
    the calling function shadows a module-level one
- **two candidates in the winning scope** are an error — the call must pass
    the argument explicitly
- in the scope containing the call, only declarations **lexically before**
    the call count. enclosing-scope declarations count regardless of position,
    matching how closed-over names are read late
- a declaration a **nearer scope shadows** is not a candidate. the call site
    passes the resolved name, so a scope in between holding that name would
    have the call read its value instead
- an **explicit argument** (positional or keyword) always wins; no resolution
    happens for a parameter that is matched

a function's own `context` parameters are declarations in its body scope, so a
requirement propagates through call chains:

```by
def f(context b: str) -> str:
    return b

def g(x: int, context b: str) -> str:
    return f()  # ok — g's own `b` fills it
```

## trailing lambda blocks

a [trailing lambda](trailing-lambdas.md) block binds the value its callback is
called with as `it`, and a block bound to an
[implicit receiver](implicit-receivers.md) spells that receiver `self`. nobody
writes either binding, so both are ambient in the block body the way a `context`
declaration is ambient in its scope — and both fill `context` parameters:

```by
def log(message: str, context level: int): ...
def at(level: int, fn: (int) -> None): ...

at(2):
    log("started")  # ok — the block's `it` fills `level`
```

only the innermost block's implicit names count. every block binds `it`, so a
nested block always shadows the one around it, and reaching past it would name a
value the call never receives. a callback both a receiver and an `it` fit is
ambiguous, like any two candidates in one scope

a block whose callback shape cannot be inspected — an unannotated callee
parameter, or one with no argument for the block to bind — leaves `it` untyped,
and an untyped `it` offers nothing rather than fitting every parameter

## parameter placement

a `context` parameter receives its implicit argument by keyword, so it must
not sit where explicit positional arguments could land on it. positional
parameters after a `context` parameter, positional-only `context` parameters,
and `*args` after a `context` parameter are parse errors. keyword-only
`context` parameters (after `*`) are unrestricted

## lowering

the emitted python is ordinary code: prefixes are stripped and every call
site passes its resolved argument explicitly, by keyword

```by
def f(a: int, context b: str): ...

context s1 = "asdf"
f(2)
```

```python
def f(a: int, b: str): ...

s1 = "asdf"
f(2, b=s1)
```

the lowering is intentionally lossy — there is no marker in the output and no
reverse transform. a round-trip degrades context calls to the explicit form,
which is also valid basedpython. python callers of transpiled code simply
pass the argument themselves

## diagnostics

- `missing-context-argument` — a `context` parameter is unmatched and nothing
    in scope fits it
- `ambiguous-context-argument` — several declarations in the winning scope fit
    it

## limitations

- resolution happens only at direct call expressions (`f(...)`) whose callee
    is a plain function or bound method with a single signature. constructors,
    `__call__` instances, overloaded functions, union-typed callees, dunder
    dispatch, and calls with `*` / `**` unpacking all keep the plain
    missing-argument behaviour — pass the argument explicitly there
- candidates are typed at their declaration site: reassigning a context
    variable to a different (assignable) type between declaration and call is
    not tracked
- a declaration inside a conditional branch counts lexically; whether the
    branch executed is not tracked

## inlay hints

a call site shows the arguments it fills implicitly, written where the lowering
writes them — after the explicit arguments, by keyword. each variable navigates
to the declaration that resolved it:

```by
def f(context a: int) -> None: ...
def g(x: str, context a: int) -> None: ...

context b = 1

f(⟨a=b⟩)
g("y"⟨, a=b⟩)
```

a parameter given explicitly is not hinted, and neither is one that failed to
resolve — `missing-context-argument` and `ambiguous-context-argument` already
say so
