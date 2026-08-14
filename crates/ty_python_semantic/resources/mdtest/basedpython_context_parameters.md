# basedpython: context parameters

A `context` parameter is filled implicitly at call sites from the `context` declarations in scope.
Resolution is by assignability, not by name: the innermost scope with at least one declaration whose
type is assignable to the parameter wins, and more than one match in that scope is an error. In the
scope containing the call only declarations lexically before the call count; enclosing-scope
declarations count regardless of position.

## missing, resolved, and ambiguous

```by
def f(a: int, context b: str): ...

f(1)  # error: [missing-context-argument]
context s1 = "asdf"
f(2)  # ok, `s1` is passed implicitly
context s2 = "fdsa"
f(3)  # error: [ambiguous-context-argument]
```

## resolution is by assignability, not name

A declaration whose type does not fit the parameter is not a candidate, so an `int` declaration
never collides with a `str` parameter.

```by
def f(context b: str): ...
def g(context n: int): ...

context s = "asdf"
context count = 1

f()
g()
```

## explicit arguments suppress resolution

```by
def f(a: int, context b: str): ...

context s1 = "asdf"
context s2 = "fdsa"

f(1, "explicit")
f(2, b="explicit")
f(3)  # error: [ambiguous-context-argument]
```

## a `context` parameter propagates through the body

A function's own `context` parameters are declarations in its body scope, so a requirement threads
through call chains without explicit forwarding.

```by
def f(context b: str) -> str:
    return b

def g(x: int, context b: str) -> str:
    return f()
```

## the innermost scope wins

A declaration in the calling function shadows a module-level one — no ambiguity.

```by
def f(context b: str): ...

context outer = "module"

def g():
    context inner = "local"
    f()
```

## typed declarations

`context NAME: T = value` declares `T`, and the value must fit it.

```by
def f(context b: int): ...

context n: int = 1
f()

context bad: str = 2  # error: [invalid-assignment]
```

## the declared type is what candidates are matched by

```by
def f(context b: str): ...

context n: int = 1
f()  # error: [missing-context-argument]
```

## keyword-only context parameters

```by
def f(a: int, *, context b: str): ...

context s = "asdf"
f(1)
```

## calls before the declaration do not see it

```by
def f(context b: str): ...

f()  # error: [missing-context-argument]
context s = "asdf"
f()
```

## enclosing-scope declarations count regardless of position

Module-level declarations are read late from a function body, like any closed-over name.

```by
def f(context b: str): ...

def g():
    f()

context s = "asdf"
g()
```

## a `context` parameter with a matching unannotated declaration

An unannotated declaration is typed by its value.

```by
def f(context b: bool): ...

context flag = True
f()
```

## a nearer scope holding the name shadows the declaration

The lowering writes the resolved name at the call site, so a scope between the call and the
declaration that binds that name would make the emitted argument read its value instead. Such a
declaration is not offered at all.

```by
def f(context b: str): ...

context s = "module"

def g():
    s = 1
    f()  # error: [missing-context-argument]
```

## a trailing lambda block's `it` is a candidate

A block binds the value its callback is called with as `it`, and nobody writes that binding. It is
ambient in the block body the way a `context` declaration is ambient in its scope, so it fills a
`context` parameter too.

```by
def f(context b: str): ...
def each(fn: (str) -> None): ...

each:
    f()
```

## a receiver block's `self` is a candidate

A block bound to a receiver callback spells the receiver `self`, which is likewise never written.

```by
def f(context b: str): ...
def against(fn: str.() -> None): ...

against:
    f()
```

## a block that binds both a receiver and `it` is ambiguous

`self` and `it` are two separate values, so a `context` parameter that both fit is no more
resolvable than two matching declarations in one scope.

```by
def f(context b: str): ...
def against(fn: str.(str) -> None): ...

against:
    f()  # error: [ambiguous-context-argument]
```

## an untyped `it` is not a candidate

A callee whose callback shape cannot be inspected leaves `it` untyped, and an untyped `it` would be
assignable to every `context` parameter — so it is not offered at all.

```by
def f(context b: str): ...
def opaque(fn): ...

opaque:
    f()  # error: [missing-context-argument]
```

## a callback with no parameters leaves `it` untyped

A callback the block has nothing to bind leaves `it` untyped for the same reason, and is likewise
not offered.

```by
def f(context b: str): ...
def once(fn: () -> None): ...

once:
    f()  # error: [missing-context-argument]
```

## only the innermost block's implicit names count

Every block binds `it`, so a nested block always shadows the enclosing one's — and the two blocks'
receivers share a name in the emitted code as well. Reaching past a nested block would name a value
the call does not receive, so an enclosing block's implicit names are not offered.

```by
def f(context b: str): ...
def outer(fn: (str) -> None): ...
def inner(fn: (int) -> None): ...

outer:
    inner:
        f()  # error: [missing-context-argument]
```

## a comprehension in the block that rebinds `it` shadows it

The block's implicit names stay ambient inside a comprehension it opens, but a comprehension that
binds `it` itself claims the name for its own loop variable.

```by
def f(context b: str): ...
def each(fn: (str) -> None): ...

each:
    print([f() for it in range(3)])  # error: [missing-context-argument]
```

## a `context` declaration in the block shadows `it`

The block's implicit names come first in its own scope, so a declaration that reuses one of their
names replaces it rather than colliding with it.

```by
def f(context b: str): ...
def each(fn: (str) -> None): ...

each:
    context it: str = "declared"
    f()
```

## `context` parameters must come last

A positional parameter after a `context` parameter would shift explicit arguments onto it.

```by
def f(context b: str, a: int): ...  # error: [invalid-syntax] "parameter after a `context` parameter must also be `context`"
```

## a `context` parameter cannot be positional-only

```by
def f(context b: str, /): ...  # error: [invalid-syntax] "a positional-only parameter cannot be a `context` parameter"
```

## `*args` cannot follow a `context` parameter

```by
def f(context b: str, *args: int): ...  # error: [invalid-syntax] "`*` parameter cannot follow a `context` parameter"
```

## resolution is limited to plain functions and bound methods

The transpiler can only inject implicit arguments where it can see a single signature, so
constructors (and other indirect callables) keep the plain missing-argument behaviour and require
explicit arguments.

```by
class A:
    def __init__(self, context b: str): ...

    def m(self, context b: str): ...

context s = "asdf"
A()  # error: [missing-argument]
a = A(s)
a.m()  # ok — bound methods resolve
```

## reveal_type of the parameter inside the body

The `context` prefix does not change the parameter's declared type.

```by
def f(a: int, context b: str):
    reveal_type(b)  # revealed: str
```
