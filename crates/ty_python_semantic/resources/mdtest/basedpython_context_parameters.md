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

## a `context _` parameter supplies a context argument

A `context` parameter named `_` is an ordinary name when it is the function's only parameter named
`_`.

```by
def show(context label: str) -> str:
    return label

def relay(context _: str) -> str:
    return show()

print(relay("a"))
```

## a repeated `_` parameter does not supply a context argument

A function may name several parameters `_`, but Python binds only one of them to the name `_`. Which
one a call site would read is not decided, so a `context` parameter whose value would come from a
repeated `_` is reported rather than filled from either of them.

```by
def show(context label: str) -> str:
    return label

def relay(context _: int, context _: str) -> str:
    # error: [missing-context-argument] "a repeated `_` parameter cannot supply context parameter `label` of function `show`"
    return show()
```

This holds for every repeated `_` that matches, not only the last one, and for a `context` `_`
repeated by an ordinary parameter.

```by
def count(context n: int) -> int:
    return n

def first(context _: int, context _: str) -> int:
    # error: [missing-context-argument] "a repeated `_` parameter cannot supply context parameter `n` of function `count`"
    return count()

def mixed(_: str, context _: int) -> int:
    # error: [missing-context-argument] "a repeated `_` parameter cannot supply context parameter `n` of function `count`"
    return count()
```

## a repeated `_` parameter is not filled implicitly

A function may name several parameters `_`, `context` ones among them. They are positional-only, so
a call passes them by position, and an implicit argument is written as a keyword, so none can be
written for one. Each such parameter the call leaves unmatched is reported, whatever is in scope.

```by
def show(context _: int, context _: str) -> str:
    return "ok"

context n = 1
context s = "a"

show(2, "b")

# error: [missing-context-argument] "repeated `_` parameter 1 of function `show` cannot be supplied implicitly"
# error: [missing-context-argument] "repeated `_` parameter 2 of function `show` cannot be supplied implicitly"
show()
```

A keyword `_` fills neither of them.

```by
# error: [positional-only-parameter-as-kwarg]
# error: [missing-context-argument] "repeated `_` parameter 2 of function `show` cannot be supplied implicitly"
show(_=2)
```

This holds for a `context` parameter `_` that repeats the name of an ordinary one.

```by
def mixed(_: int, context _: str) -> str:
    return "ok"

mixed(1, "b")

# error: [missing-context-argument] "repeated `_` parameter 2 of function `mixed` cannot be supplied implicitly"
mixed(1)
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

## `context let` declares a `Final` candidate

`context` prefixes a declaration rather than forming one of its own, so the rest of the modifier
chain decides what the declaration is and `context` only adds the candidacy.

```by
def f(context b: str) -> str:
    return b

context let s: str = "asdf"
f()

# error: [invalid-assignment] "read-only symbol `s` cannot be reassigned"
s = "reassigned"
```

## the modifiers may be written in either order

```by
def f(context b: str) -> str:
    return b

def g(context n: int) -> int:
    return n

context var count: int = 1
private context let name = "asdf"

f()
g()
```

## a declaration may be named after a modifier keyword

A modifier keyword is only a modifier where a modifier can stand. Directly in front of the `=` or
the `:` it is the name being declared, which is the only reading that leaves the declaration with
one.

```by
def f(context b: int) -> int:
    return b

def g(context c: str) -> str:
    return c

context data = 1
context final: str = "asdf"

f()
g()
```

## `context` is not a modifier on a definition

```by
# error: [invalid-syntax] "`context` is not a modifier on a `def` or a `class`"
context def f(): ...
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

The transpiler can only inject implicit arguments where it can see the callee's own signature, so
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

## an overloaded callee fills a parameter every overload agrees on

Which overload a call selects is decided by the arguments it is given, so an argument may be written
only where it is the right one for every overload at once: the same name, resolved to the same
value, and keyword-only in all of them so that whether the call already supplies it cannot depend on
which one is selected.

```by
from typing import overload

@overload
def f(a: int, *, context b: str) -> int: ...
@overload
def f(a: str, *, context b: str) -> str: ...
def f(a: int | str, *, context b: str) -> int | str:
    return a

context t: str = "hello"

reveal_type(f(1))  # revealed: int
```

## overloads that disagree on a `context` parameter fill nothing

<!-- snapshot-diagnostics -->

The name means a different thing in each overload, so no one argument is right whichever is
selected. Nothing is written, and a parameter with no default is the missing argument it is.

The call then goes without an argument nothing was ever going to write, so the report says which
overloads parted and what each of them would have done.

```by
from typing import overload

@overload
def f(a: int, *, context b: str) -> int: ...
@overload
def f(a: str, *, context b: int) -> str: ...
def f(a: int | str, *, context b: str | int) -> int | str:
    return a

context t: str = "hello"
context u: int = 1

f(1)  # error: [no-matching-overload]
```

## an overload that leaves the `context` parameter out

<!-- snapshot-diagnostics -->

Declaring the parameter in only some overloads is a disagreement of the same kind: a call that
selects the overload without it would be given an argument it never asked for.

```by
from typing import overload

@overload
def g(a: int, *, context b: str) -> int: ...
@overload
def g(a: int, x: int) -> str: ...
def g(a: int, x: int = 0, *, context b: str = "") -> int | str:
    return a

context t: str = "hello"

g(1)  # error: [no-matching-overload]
```

## a positional `context` parameter of an overload set is not filled

<!-- snapshot-diagnostics -->

A positional slot can sit at a different index in each overload, so whether the call already fills
it is not the same question for all of them. Only a keyword-only parameter is read through an
overload set.

The call then goes without an argument nothing was ever going to write, which on its own reads as an
ordinary failure to match. So the report names the parameter and the spelling that would have
worked.

```by
from typing import overload

@overload
def f(a: int, context b: str) -> int: ...
@overload
def f(a: str, context b: str) -> str: ...
def f(a: int | str, context b: str) -> int | str:
    return a

context t: str = "hello"

f(1)  # error: [no-matching-overload]
```

## a decoration cannot fill a `context` parameter

`@deco` is a call — it runs `deco(g)` — but it is the one call the source writes no argument list
for, so there is nowhere to put the implicit argument. A parameter with a default would quietly take
it rather than the ambient value the declaration promised, so the decoration is reported and left as
written. The call form is where the argument can be written.

```by
def deco(fn: (...) -> object, context b: str = "default") -> object:
    return fn

context t: str = "hello"

# error: [missing-context-argument] "context parameter `b` cannot be filled at a decoration"
@deco
def g(): ...

def undecorated(): ...

def call_it() -> object:
    return deco(undecorated)  # ok — `t` is passed implicitly
```

The decorated definition is the decorator's first positional argument, so a `context` parameter
standing in that slot is filled by it like any other.

```by
def only(context fn: object) -> object:
    return fn

@only
def h(): ...
```

## a decoration of a `decorator def` cannot fill one either

A `decorator def` is declared as the pair of overloads it is applied in, and the option is unfilled
in both — so which one the decoration selects does not change the answer.

```by
decorator def d(fn: (...) -> object, context tag: str = "default") -> object:
    print("decorating with", tag)
    return fn

context t: str = "hello"

# error: [missing-context-argument] "context parameter `tag` cannot be filled at a decoration"
@d
def g(): ...
```

## an overload that fills the parameter leaves the decoration alone

Only a parameter unfilled in every overload is reported: which overload a decoration selects is not
decided here, so a report has to be true whichever it is.

```by
from typing import overload

@overload
def deco(fn: (...) -> object, context b: str = "default") -> object: ...
@overload
def deco(fn: int) -> object: ...
def deco(fn: object, b: str = "default") -> object:
    return fn

context t: str = "hello"

@deco
def g(): ...
```

## reveal_type of the parameter inside the body

The `context` prefix does not change the parameter's declared type.

```by
def f(a: int, context b: str):
    reveal_type(b)  # revealed: str
```
