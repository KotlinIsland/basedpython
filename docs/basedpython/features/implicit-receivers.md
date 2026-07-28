# implicit receivers

a callable type may declare a *receiver*: `int.() -> str` is a callable that runs
*against* an `int`. the receiver is the callable's leading positional parameter,
so nothing about the callable itself is special — any function of that shape
satisfies it, and it can be called directly:

```by
def render(value: int) -> str:
    return str(value)

def apply(fn: int.() -> str) -> str:
    return fn(1)

apply(render)
```

what the receiver adds is two ways of reading it back out

## calling through the receiver

a name in scope declared as a receiver callable can be called as a method of a
matching receiver:

```by
def apply(fn: int.() -> str):
    receiver = 1
    receiver.fn()  # `fn(receiver)`
```

`receiver.fn` on its own is the callable with the receiver already supplied —
`() -> str`. it lowers to a `functools.partial`, exactly as a bound method would
carry its receiver:

```python
bound = functools.partial(fn, receiver)
```

resolution is a *last* fallback, so nothing that resolves today changes meaning:

- a real member of the receiver type always wins (`(1).bit_length()` is
    untouched, even with a `bit_length` receiver callable in scope)
- an [extension](extensions.md) member wins over a receiver callable
- the name must be **declared** — a receiver callable is only ever spelled as an
    annotation, and a declaration means the same thing everywhere it is visible
- the receiver must be assignable to the callable's receiver parameter
- a scope that binds the name to anything else shadows it, exactly as it would
    shadow an ordinary load of that name

an access on an [optional chain](optional-chaining.md) (`a?.fn()`) is rejected —
the chain lowers to its own conditional, which the receiver rewrite cannot yet be
spliced into

## trailing lambda blocks

when a [trailing lambda](trailing-lambdas.md) block fills a receiver callback,
the block binds that receiver itself. the body sees the receiver's members
unqualified, and spells the receiver `self`; the block's implicit `it` parameter
is the callback's *own* argument, the one after the receiver:

```by
def apply(fn: int.(str) -> None):
    fn(1, "a")

apply:
    print(self)  # 1
    print(imag)  # 0 — a member of `self`
    print(it)    # "a"
```

→

```python
def _trailing_lambda_0(_by_self=None, it=None):
    print(_by_self)
    print(_by_self.imag)
    print(it)
apply(fn=_trailing_lambda_0)
```

the receiver lands in a parameter the source cannot spell, so nothing the block
binds can redirect the members read off it

as with `x.fn`, this is the last fallback. a name bound anywhere in the lexical
chain — a block local, an enclosing function's local, a module global, a builtin —
keeps its ordinary meaning, so a block can never capture a name out from under
the scope around it:

```by
imag: str = "shadow"

apply:
    print(imag)  # the module-level `imag`, not the receiver's
```

`self` is no exception: inside a method, `self` is that method's own receiver,
and the block's receiver is reachable only through its members

a name that resolves nowhere and is not a member of the receiver stays an
`unresolved-reference` error

a block still returns `None`, so the callback must be declared to return a type
that accepts it — `int.() -> None`, not `int.() -> str` (see
[trailing lambdas](trailing-lambdas.md#binding)) — and it binds one argument
beyond the receiver, so `int.(str, str) -> None` is rejected with
`trailing-lambda-parameters`

## syntax

the receiver precedes the parameter list, separated by a `.`:

```by
a: int.() -> str                       # receiver only
b: str.(int) -> bytes                  # receiver plus parameters
c: int.(str, *, flag: bool) -> None    # any parameter form
d: list[int.() -> str]                 # nests like any type expression
e: int.() -> str.() -> bytes           # the return type may be one too
```

`.` followed by `(` is never valid python, so the form is unambiguous. like the
[callable arrow](callable.md) it is parsed anywhere an expression is, and is
meaningful only in a type expression; a value-position one is a syntax error in
`.py` files and has no type in `.by` files

## lowering

the receiver is the leading positional parameter of the lowered type:

| basedpython            | python                                                  |
| ---------------------- | ------------------------------------------------------- |
| `int.() -> str`        | `Callable[[int], str]`                                  |
| `int.(str) -> bytes`   | `Callable[[int, str], bytes]`                           |
| `int.(**P) -> str`     | `Callable[Concatenate[int, P], str]`                    |
| `int.(a: str) -> None` | a `Protocol` whose `__call__` takes `(_receiver, /, a)` |
| `int.(...) -> str`     | `Callable[..., str]`                                    |

the gradual form is the one lossy case: `Callable[..., str]` already accepts the
receiver-first call, and `Concatenate[int, ...]` is not spellable on every
supported python version. the receiver is still a real parameter to the checker

reverse transpiling never *produces* the receiver form — a lowered
`Callable[[int], str]` reads back as `(int) -> str`, since which parameter was
the receiver is not recoverable
