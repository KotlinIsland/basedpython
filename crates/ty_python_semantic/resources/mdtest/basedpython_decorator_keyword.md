# basedpython: `decorator def`

basedpython exposes a `decorator` soft keyword on `def`. it declares a function whose first
positional parameter is the decorated callable and whose remaining parameters are keyword-only
options. the transpile expands the declaration into two `@overload` stubs plus a runtime dispatcher

```toml
[environment]
python-version = "3.12"
```

## body type-checks against declared parameters

```by
from typing import Callable

decorator def d(fn: Callable[..., object], option: bool = False) -> int:
    reveal_type(fn)  # revealed: (...) -> object
    reveal_type(option)  # revealed: bool
    return 1 if option else len(str(fn))
```

## direct decoration applies the declared return type

`@d` applied directly to a function calls `d(fn)`, which returns the function's declared return
type. the synthetic decorator that drives the transpile is invisible to ty — function `d` appears
with its original signature

```by
from typing import Callable

decorator def d(fn: Callable[..., object], option: bool = False) -> int:
    return 1 if option else len(str(fn))

def g() -> str:
    return "x"

reveal_type(d(g))  # revealed: int
reveal_type(d(g, option=True))  # revealed: int
```

## no options — single positional parameter

```by
from typing import Callable

decorator def trace(fn: Callable[..., object]) -> int:
    return len(str(fn))

def h() -> None: ...

reveal_type(trace(h))  # revealed: int
```

## applied with options

A `decorator def` is applied in three shapes: bare, with empty parentheses, and with options. The
parentheses call it with the options alone, and what that returns is what decorates the function, so
every shape gives the function the decorator's declared return type.

```by
decorator def d(fn: (int) -> None, option: bool = False) -> str:
    return "on" if option else "off"

def plain(i: int) -> None: ...

@d
def f1(i): ...

@d()
def f2(i): ...

@d(option=True)
def f3(i): ...

reveal_type(f1)  # revealed: str
reveal_type(f2)  # revealed: str
reveal_type(f3)  # revealed: str

reveal_type(d(plain))  # revealed: str
reveal_type(d(option=True))  # revealed: (fn: (int, /) -> None, /) -> str
```

An option is still checked, in either shape.

```by
# error: [invalid-argument-type] "Expected `bool`, found `"yes"`"
# error: [dynamic-function-decorator-return]
@d(option="yes")
def f4(i): ...

# error: [invalid-argument-type] "Expected `bool`, found `"yes"`"
d(plain, option="yes")
```

The options are keyword-only at the call site: one dispatcher serves every shape, and it tells them
apart by whether it was given the decorated function positionally.

```by
d(plain, True)  # error: [no-matching-overload]
```

## the decorated function's unannotated parameters take their declared types

a decoration hands the function to the decorator, so the callable the declaration says it accepts is
type context for the decorated function's parameters. this is the same context that types the
parameters of a lambda passed to `d` directly, and it is not particular to `decorator def` — see
`bidirectional.md`

```by
decorator def d(fn: (int) -> None): ...

@d
def f(i):
    reveal_type(i)  # revealed: int
```

Applying it with options declares them just as directly: the options are matched first, and what the
call returns takes the function.

```by
decorator def e(fn: (str) -> None, option: bool = False): ...

@e(option=True)
def g(s):
    reveal_type(s)  # revealed: str
```

## multiple options

```by
from typing import Callable

decorator def configure(
    fn: Callable[..., object],
    name: str = "default",
    count: int = 0,
) -> str:
    return name * count

def target() -> None: ...

reveal_type(configure(target))  # revealed: str
reveal_type(configure(target, name="hello", count=2))  # revealed: str
```

## a declaration with no parameter for the decorated function

The lowering builds the dispatcher and its two overloads out of a first parameter that receives the
decorated function. A declaration with no parameters has nothing to hand the function to, and is
refused rather than transpiled into a decorator that cannot be applied.

```by
# error: [invalid-decorator-def] "`decorator def logged` declares no parameter for the function it decorates"
decorator def logged() -> int:
    return 1
```

## a default on the decorated parameter

Every shape a `decorator def` is applied in supplies the decorated function, so a default on the
parameter that receives it stands for a call that never happens.

```by
def nothing() -> None: ...

# error: [invalid-decorator-def] "The decorated parameter `fn` of `decorator def retry` cannot have a default"
decorator def retry(fn: (...) -> object = nothing) -> int:
    return 1
```

## an option with no default

The options are what `@d` decorates *without*, so each of them has to have a default. One that does
not would leave the bare and empty-parenthesis shapes with an argument missing.

```by
# error: [invalid-decorator-def] "Option `label` of `decorator def traced` has no default"
decorator def traced(fn: (...) -> object, label: str) -> int:
    return 1
```

## a variadic parameter

Neither the decorated function nor an option arrives through `*args` or `**kwargs`: the dispatcher
tells the two call shapes apart by whether it was given the function positionally, and a variadic
absorbs both.

```by
# error: [invalid-decorator-def] "`decorator def timed` cannot declare `*args` or `**kwargs`"
decorator def timed(fn: (...) -> object, *rest: int) -> int:
    return 1
```

## inside a class body

`decorator def` is module-scope only. In a class body the overloads the lowering writes would stand
in the class's own namespace, as attributes rather than declarations, so the keyword is refused
there and a decorator written as a method is a plain `def` that returns a callable.

```by
class Registry:
    # error: [invalid-decorator-def] "`decorator def register` is only valid at module scope"
    decorator def register(fn: (...) -> object) -> int:
        return 1
```

## a declaration the lowering refuses keeps the signature it was written with

Nothing is expanded, so the name stands for the function the source declared. Reading it that way is
what makes the refusal above the only thing reported.

```by
# error: [invalid-decorator-def]
decorator def logged(fn: (...) -> object, level: int) -> str:
    return "x"

def target() -> None: ...

reveal_type(logged(target, 3))  # revealed: str
```
