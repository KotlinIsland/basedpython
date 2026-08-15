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

### priority

unlike `x.fn`, this is not a fallback. the receiver joins the scope chain at the
block's own level — *inside* the names the block itself binds, and *outside*
everything else — so its members win over an enclosing function's local, a module
global and a builtin alike:

```by
imag: str = "shadow"

apply:
    print(imag)  # the receiver's `imag`, not the module-level one
```

only the block itself outranks it:

```by
apply:
    imag = "block"
    print(imag)  # "block"
```

`self` is no exception. inside a method the block's `self` is the block's
receiver, and the method's own receiver is not reachable from the body

a call is the one thing that can turn the receiver down. a name used as a callee
takes the receiver's member only if that member accepts the call, and otherwise
carries on outwards to whatever else declares the name:

```by
class Repeater:
    def emit(self, times: int): ...

def apply(fn: Repeater.() -> None): ...

def emit(label: str, times: int): ...

apply:
    emit(2)        # `self.emit`
    emit("a", 2)   # the module-level `emit`
```

what counts is the *shape* of the call — how many positional arguments it passes
and which keywords — never the types of the arguments. two candidates that differ
only in what their parameters accept do not disambiguate this way; the receiver's
wins and the call is checked against it.

if no candidate anywhere accepts the call, the receiver's is used, so the call
reports its own mismatch rather than an unresolved name. a name that resolves
nowhere and is not a member of the receiver stays an `unresolved-reference` error

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
