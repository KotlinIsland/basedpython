# decorator keyword

`decorator def` declares a function that can be used as a decorator in three call
shapes — bare, with empty parens, and with options:

```by
decorator def d(fn: (...) -> object, option: bool = False) -> int:
    return 1 if option else len(str(fn))

@d
def f1(): ...

@d()
def f2(): ...

@d(option=True)
def f3(): ...
```

## call shapes

- `@d` — direct decoration
- `@d()` — parens, no options
- `@d(opt=...)` — parens with options

the first positional parameter is the decorated callable. all other parameters must be
keyword-only and have defaults — they are the decorator's options

## rules

- the function must have at least one positional parameter — the decorated callable
- any remaining parameters are made keyword-only at the call site, and must have
    defaults
- the `fn` parameter's declared type is what a decoration is checked against, and
    is what gives the decorated function's parameters their types — see
    [decorated function parameters](decorated-parameters.md)
- the return type of the user-written function is preserved as the result type of
    applying the decorator

## declaring one without a body

like any other `def`, a `decorator def` can be written with no body at all — a
declaration of the shape, with nothing to run

```by
decorator def route(fn: (int) -> None)
```

## scope

`decorator def` is **module-scope only**. inside a class body the keyword is
rejected — class-method decorators don't need the keyword (use a normal
`def` returning a callable), and the synthesized overloads would shadow the
enclosing class's attribute namespace

## why a keyword

a hand-written decorator that supports all three call shapes is tedious and easy to get
wrong (sentinel handling, overload ordering, recursive dispatch). the keyword removes
boilerplate and centralizes the pattern so the overloads always match the runtime impl
