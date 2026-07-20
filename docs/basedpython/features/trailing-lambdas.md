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
`(int) -> str`, so `it` is `int`. the block's return value is deliberately
unchecked — blocks are written for effect

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

ty's flow analysis reflects the write too: because the block runs inline at the
call site, its assignments narrow the enclosing binding, so a `reveal_type`
after the block sees the block's value, not the value before it:

```by
def f(fn: () -> None):
    fn()

def main():
    a: int = 1
    f:
        a = 2
    reveal_type(a)  # 2, not 1
```

this treats the block as running once at the `f:` point (like a `with` body).
a *conditional* assignment unions with the prior value — `if c(): a = 2` leaves
`a` as `1 | 2`, because the block runs but the write itself might not; only a
write that happens on every branch drops the prior. a callback the callee never
actually calls is the same accepted imprecision as a `nonlocal` write ty can't
prove happens

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
