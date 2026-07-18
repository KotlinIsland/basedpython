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
