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

customising the lambda's parameters is not supported yet — the block always
takes exactly one parameter named `it`, defaulted to `None` so a callback whose
type takes no argument (`() -> None`, invoked as `fn()`) can still call it

## binding

the block binds the callee's *last declared parameter*, passed by keyword when
ty can inspect the callee's signature. that is what lets `f:` above leave the
defaulted `x` untouched. when the signature is not inspectable (an unresolved
import, a `*args` last parameter, a positional-only parameter) the block is
appended as the last positional argument instead

`it` is context-typed from the callee: the sole positional parameter of the
callable the last parameter is declared as. in the example above `a` is
`(int) -> str`, so `it` is `int`.

the block itself always returns `None` — in a `once` block a `return` targets
the *enclosing* function, not the block (see [once blocks](#once-blocks)) — so
the callee's callback must be declared to return a type that accepts `None`
(`None`, `int | None`, `object`). a callback declared to return anything else is
rejected with `trailing-lambda-return-type`; those return types are not yet
supported

the call is type-checked with the block counted as an argument: missing
earlier arguments, an over-supplied last parameter, or a non-callable target
are all reported:

```by
def g(x: int, a: (int) -> str):
    a(x)

g:  # error: missing argument for `x`
    print(it)
```

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
(`int.() -> None`), the block body sees that type's members unqualified — `imag`
means `it.imag`. a name bound anywhere in the lexical chain keeps its ordinary
meaning, so only names that would otherwise be unresolved resolve this way

## enclosing scope

a block shares the enclosing scope for its assignments: writing to a name that
is already bound outside the block updates that binding instead of shadowing it
with a fresh block local. the lowering inserts the `global` / `nonlocal`
declaration the closure needs, so no manual `nonlocal` is required:

```by
a: int = 1
f:
    a = 2
print(a)  # 2
```

→

```python
a: int = 1
def _trailing_lambda_0(it=None):
    global a
    a = 2
f(a=_trailing_lambda_0)
print(a)
```

a module-level binding is captured with `global`, an enclosing function's local
with `nonlocal`. a name bound in no enclosing scope stays a plain block local,
and an attribute or item target (`obj.x = …`) rebinds no name, so neither is
declared

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

the block's implicit `it` parameter is shown as an inlay hint with the type the
callee gives it, written where a parameter list would go:

```by
def apply(fn: (int) -> None) -> None:
    fn(1)

apply⟨it: int⟩:
    print(it)
```

the same hint covers the other parameters basedpython synthesizes rather than
spells — an [`init(...)`](init-method.md) method's receiver, and a
[property accessor](properties.md)'s:

```by
class C:
    init(⟨self⟩a: int)
```
