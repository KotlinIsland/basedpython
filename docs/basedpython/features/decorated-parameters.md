# decorated function parameters

a decoration hands the function to the decorator, so the callable the decorator declares it
accepts gives the decorated function's unannotated parameters their types

```by
decorator def route(fn: (int) -> None)

@route
def home(request):
    reveal_type(request)  # int
```

it is the same context a lambda written at that argument position is inferred under —
`route(lambda (request): None)` types `request` the same way

## any decorator declares it

nothing about this is specific to [`decorator def`](decorator-keyword.md); an ordinary function
used as a decorator declares the same thing

```by
def route(fn: (int) -> None): ...

@route
def home(request):
    reveal_type(request)  # int
```

parameters correspond by position, so a function that takes more parameters than the declared
callable leaves the surplus ones untyped — and the decoration itself is an
`invalid-argument-type`, since a function of that shape is not what the decorator accepts

a parameter that carries its own annotation keeps it, and is checked against the declared
callable like any other argument

```by
@route
def home(request: str): ...   # error: invalid-argument-type
```

a callback protocol is read the same way, and is the only spelling that can declare a keyword-only
parameter. those correspond by name rather than by position

```by
protocol Handler:
    def __call__(self, request: int, *, verbose: str): ...

def route(fn: Handler): ...

@route
def home(request, *, verbose):
    reveal_type(request)  # int
    reveal_type(verbose)  # str
```

## when nothing is declared

a decorator that accepts a function of any shape says nothing about any particular parameter, and
the parameter falls back to what it would have been undecorated — under
[sound types](sound-types.md), an anonymous type parameter

```by
def log[**P, R](fn: (**P) -> R) -> (**P) -> R:
    return fn

@log
def home(request):
    reveal_type(request)  # request@home
```

this covers the gradual `(...) -> T`, a parameter pack, and any declared parameter type written in
terms of the decorator's own type variables — a type variable belongs to the scope that declared
it, so it cannot stand as a parameter type here

an overloaded decorator declares nothing either: which overload a decoration picks depends on the
shape of the function, which is the very thing being inferred

## more than one decorator

decorators apply from the bottom up, so the parameters are typed by the one written closest to the
`def` — the decorators above it see whatever the one below them returned

```by
def inner(fn: (int) -> None) -> (str) -> None:
    return lambda (s): None

def outer(fn: (str) -> None): ...

@outer
@inner
def home(request):
    reveal_type(request)  # int
```

a decorator that only marks the function — `@overload`, [`final`, `override`,
`static`](modifiers.md) — leaves its type as it was, so it is not what decorates it and the
decorator above it still declares the parameters
