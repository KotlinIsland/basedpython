# trailing lambda blocks

a statement-level expression followed by `:` and an indented suite calls that
expression with the suite as its last argument. the suite becomes a function
taking the single implicit parameter `it` — kotlin-style trailing lambdas:

```by
def f(x: int = 1, a: (int) -> str):
    a(x)

f:
    print(it)  # prints 1

f(2):
    print(it)  # prints 2
```

customising the lambda's parameters is not supported yet — the block takes
exactly one parameter named `it` (plus the callback's
[receiver](#implicit-receivers), where it declares one). a callback that takes
*more* than that one argument is rejected with `trailing-lambda-parameters`: the
extra arguments have no parameter to land in, and no spelling in the body

```by
def f(a: (int, str) -> None):
    a(1, "two")

f:  # error: the block binds only `it`, so `"two"` has nowhere to go
    print(it)
```

every block has an `it`, whatever its callback passes — the lambda the lowering
writes always declares the parameter, so inside a block `it` always means that
parameter and never a name from an enclosing scope. a callback that takes
*nothing* is invoked as `a()`, though, so nothing ever fills it, and reading it
reads the `None` default rather than a value the call sent:

```by
def g(a: () -> None):
    a()

g:
    print(it)  # error[trailing-lambda-parameters] — `a` passes no argument

g:
    print("hi")  # fine — the block just doesn't take one
```

a body that writes the name first is reading its own local rather than the
parameter, so such a block is free to use `it` for something of its own. a
callback whose shape cannot be read — a gradual `(...) -> None`, a
`Callable[...]` spelling — says nothing either way, and leaves `it` untyped and
unreported

## as a value

a block can stand as an assignment's value, where it binds the call it stands
for — not the callee:

```by
def f(x: int, a: (int) -> None) -> str:
    a(x)
    return "done"

result = f(2):
    print(it)

reveal_type(result)  # str
```

→

```python
def _trailing_lambda_0(it=None):
    print(it)
result = f(2, a=_trailing_lambda_0)
```

an annotation works the same way (`result: str = f(2):`), as does a declaration
(`let result = f(2):`, `var result = f(2):`). the target has to be a single name,
though: a block's value is worked out together with the binding its target makes,
and only one binding can do that — so neither a chain (`a = b = f:`) nor an
unpacking (`a, b = f:`) takes a block

a `return` takes one too, so a builder api reads as one expression:

```by
def page() -> str:
    return f(2):
        print(it)
```

the `def` the block lowers to is emitted in front of the `return`, so the value
is the call's however far down the suite the block was written

these are the positions a suite can stand in. an argument list is not one of
them — a block is delimited by indentation, and python suppresses the newline
that starts an indented suite inside brackets — so an inner call keeps its own
statement (`page.body:`) rather than nesting as `render(body: …)`

## binding

the block binds the callee's *last declared parameter*, passed by keyword when
ty can inspect the callee's signature. that is what lets `f:` above leave the
defaulted `x` untouched. when the signature is not inspectable (an unresolved
import, a `*args` last parameter, a positional-only parameter) the block is
appended as the last positional argument instead

`it` is context-typed from the callee: the sole positional parameter of the
callable the last parameter is declared as. in the example above `a` is
`(int) -> str`, so `it` is `int`.

the block itself returns `None` — in a `once` block a `return` targets the
*enclosing* function, not the block (see [once blocks](#once-blocks)) — so the
callee's callback must be declared to return a type that accepts `None` (`None`,
`int | None`, `object`). a callback declared to return anything else is rejected
with `trailing-lambda-return-type`; those return types are not yet supported. a
block whose body awaits returns the coroutine calling it produces instead — see
[async blocks](#async-blocks)

the call is type-checked with the block counted as an argument: missing
earlier arguments, an over-supplied last parameter, or a non-callable target
are all reported:

```by
def g(x: int, a: (int) -> str):
    a(x)

g:  # error: missing argument for `x`
    print(it)
```

the parameter the block lands in has to be able to hold one. a callee whose last
parameter is not callable takes the block as an ordinary argument, where nothing
the block's body says ever runs — so that is an error rather than a silent drop:

```by
def br(src: str? = None, extra: dict[str, str]? = None): ...

br:  # error: expected `dict[str, str] | None`, found `(...) -> Unknown`
    print("this block has nowhere to go")
```

the block's own shape stays gradual in that check: its `it` parameter is typed
from the callback, and its return is checked by `trailing-lambda-return-type`,
so neither is re-checked as an argument

## lowering

the block lowers to a named function followed by the call:

```python
def _trailing_lambda_0(it=None):
    print(it)
f(2, a=_trailing_lambda_0)
```

comments and nested lowerings inside the block and the call arguments are
preserved in place

## implicit receivers

when the callback declares an [implicit receiver](implicit-receivers.md)
(`str.(int) -> None`), the block binds that receiver itself, ahead of `it`: the
body sees the receiver's members unqualified, spells the receiver `self`, and
`it` is the callback's own argument — the one *after* the receiver. the receiver
joins the scope chain at the block's own level, so its members outrank every
binding outside the block — see
[priority](implicit-receivers.md#priority)

```by
def against(fn: str.(int) -> None):
    fn("asdf", 1)

against:
    print(upper(), it, self)  # ASDF 1 asdf
```

## enclosing scope

a block shares the enclosing scope for its assignments: writing to a name that
is already bound outside the block updates that binding instead of shadowing it
with a fresh block local. the lowering inserts the `global` / `nonlocal`
declaration the closure needs, so no manual `nonlocal` is required:

```by
n: int = 1
f:
    n = 2
print(n)  # 2
```

→

```python
n: int = 1
def _trailing_lambda_0(it=None):
    global n
    n = 2
f(a=_trailing_lambda_0)
print(n)
```

(`a` is `f`'s last parameter, from the definition at the top of the page — the
keyword names the callback slot, not anything the block assigns)

a module-level binding is captured with `global`, an enclosing function's local
with `nonlocal`. a name bound in no enclosing scope stays a plain block local,
and an attribute or item target (`obj.x = …`) rebinds no name, so neither is
declared

a name the block's [receiver](implicit-receivers.md) has a member for is the one
exception: writing it sets the member, so it is an attribute write and captures
nothing, whatever is bound outside. that is the same order reads follow — the
receiver outranks every binding outside the block — so both sides of the `=` mean
the same thing. `let` is how you ask for a local instead

ty's flow analysis reflects the write too. a `once` block runs exactly once at
the call site (like a `with` body), so an unconditional assignment narrows the
enclosing binding definitely — a `reveal_type` after the block sees the block's
value, not the value before it:

```by
def f(once fn: () -> None):
    fn()

def main():
    a: int = 1
    f:
        a = 2
    reveal_type(a)  # 2, not 1
```

a *conditional* assignment unions with the prior value — `if c(): a = 2` leaves
`a` as `1 | 2`, because the block runs but the write itself might not; only a
write that happens on every branch drops the prior.

a **non-`once`** callback may run any number of times (including zero), so even
an unconditional write unions with the prior value (`a` stays `1 | 2`). the
`once`-ness that drives this narrowing is resolved syntactically while the
semantic index is built — before type inference — so it is recognised only for a
**same-file** callee whose `def` is visible; an imported `once` callee is treated
conservatively as non-`once` for narrowing (the union is sound, just wider),
though the runtime lowering, which runs after inference, still honours it. a
callback the callee never actually calls is the same accepted imprecision as a
`nonlocal` write ty can't prove happens

## once blocks

when the callee marks its callback parameter [`once`](local-lifetimes.md#once-callbacks),
the block runs exactly once, with `with`-body semantics. that unlocks three
behaviours a non-`once` block (an ordinary closure, run any number of times)
does not get:

- a `return` in the block targets the **enclosing** function. the lowering
    carries the returned value out in a one-element cell and re-returns it after
    the call: `return it` becomes `cell.append(it); return`, and the enclosing
    function runs `if cell: return cell[0]` once the call comes back. a non-`once`
    block's `return` would only leave the closure, so it is rejected with
    `trailing-lambda-control-flow`
- a name the block unconditionally binds but no enclosing scope binds **survives**
    the block as an enclosing local; from a non-`once` block it stays a block local
    (it might never be bound)
- an enclosing `let` / `final` may not be assigned from a non-`once` block
    (`invalid-assignment`), since a repeated run would rebind the `Final`

> **caveat** — because the block is a real closure passed to the callee, the
> return is not a true stack unwind: the callee still finishes its own body after
> invoking the callback (this is what runs a resource manager's cleanup, matching
> `with`), and a callee that swallows the callback's control flow — or never
> calls it — defeats the propagation. tightening `return` / `break` / `continue`
> out of a `once` block into a guaranteed unwind is tracked as future work

## async blocks

a block whose body awaits is a coroutine function of its own, so it lowers to an
`async def` and the callback it fills is declared to return an awaitable. that
is what lets a scoped resource — a socket, a subprocess, a database handle — be
written as a block:

```by
async def scope(name: str, once block: () -> Awaitable[None]):
    try:
        await block()
    finally:
        await close(name)

async def main():
    await scope("db"):
        await query("select 1")
```

```python
async def main():
    async def _trailing_lambda_0(it=None):
        await query("select 1")
    await scope("db", block=_trailing_lambda_0)
```

`await` in the block is a statement about the *block*, not about the `def` it was
written inside: the block is its own function. writing `await` in front of the
call is what hands the coroutine the callee returns back to the event loop — the
block hangs off the call, and the `await` stays where it was written

## borrowed `it`

when the callee declares its callback's parameter [`local` or `once`](local-lifetimes.md#borrowed-callback-arguments)
— `def f(fn: (local Resource) -> None)` — the block is the implementation of that
callback, so the value bound to `it` is borrowed for the duration of the call and
may not escape the block:

```by
def f(fn: (local Resource) -> None):
    with acquire() as resource:
        fn(resource)

var kept: Resource | None = None
f:
    borrow(it)  # fine — re-lent to another borrow
    kept = it   # error[escaping-local] — the write-back outlives the call
```

the escape routes are the ones [`escaping-local`](local-lifetimes.md) checks
everywhere else, with the block's write-through counting as a store: a name an
enclosing scope binds, and — from a `once` block, where such a name survives —
one only the block binds. a `once` on the callback's parameter also puts the
exactly-once obligation on the block. a callee whose callback shape cannot be
inspected leaves the block unconstrained

## required parameters after defaulted ones

so a trailing block can bind the last parameter while earlier parameters keep
defaults, a `def` (not a `lambda`) may declare a required parameter after a
defaulted one. python rejects that shape at runtime, so the required parameter
is lowered to a `_MISSING` sentinel default plus a guard that raises —
mirroring python's own error:

```python
def f(x=1, a=_MISSING):
    if a is _MISSING:
        raise TypeError("f() missing required argument: 'a'")
    ...
```

the checker still treats the parameter as required. see
[mutable defaults](mutable-defaults.md) for the sentinel machinery this shares

## inlay hints

the parameters the block binds are shown as an inlay hint with the types the
callee gives them, written just past the `:` that opens the suite:

```by
def apply(fn: (int) -> None) -> None:
    fn(1)

apply:⟨it: int⟩
    print(it)
```

a callback that declares an [implicit receiver](implicit-receivers.md) runs
*against* a value as well as being passed one, so the receiver is hinted first,
spelled `self` — with `it` after it whenever the callback takes an argument of
its own:

```by
def against(fn: str.() -> None) -> None:
    "a".fn()

def against_with(fn: str.(int) -> None) -> None:
    "a".fn(1)

against:⟨self: str⟩
    print(upper())

against_with:⟨self: str, it: int⟩
    print(upper(), it)
```

the same hint covers the other parameters basedpython synthesizes rather than
spells — an [`init(...)`](init-method.md) method's receiver, and a
[property accessor](properties.md)'s. it carries the separator it would need as
source, so accepting it reads correctly:

```by
class C:
    init(⟨self, ⟩a: int)

class D:
    init(⟨self⟩)
```
