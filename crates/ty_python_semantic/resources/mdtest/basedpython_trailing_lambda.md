# basedpython: trailing lambda blocks

A statement-level expression followed by `:` and an indented suite calls that expression with the
suite as its last argument. The suite becomes a function taking the single implicit parameter `it`,
passed by keyword to the callee's last declared parameter (so earlier defaulted parameters keep
their defaults), or appended positionally when the callee's signature is not inspectable.

## the block is the call's last argument, `it` is context-typed

`it` takes the sole positional parameter type of the callable the callee's last parameter is
declared as.

```by
def f(x: int, a: (int) -> None):
    a(x)

f(2):
    reveal_type(it)  # revealed: int
```

## the callback must return `None`

A trailing-lambda block lowers to a function returning `None` (in a `once` block a `return` targets
the enclosing function, not the block), so a callback declared to return anything else cannot be
satisfied. Non-`None` return types are not yet supported.

```by
def f(a: (int) -> str):
    print(a(1))

f:  # error: [trailing-lambda-return-type] "a trailing-lambda callback must return `None`, not `str` (other return types are not yet supported)"
    print(it)
```

A return type that merely accepts `None` — `None` itself, `int | None`, `object` — is fine.

```by
def g(a: (int) -> None):
    a(1)

g:
    print(it)
```

## earlier defaulted parameters keep their defaults

A required parameter may follow a defaulted one in a `def` — the trailing block binds the last
parameter by keyword, so `x` keeps its default.

```by
def f(x: int = 1, a: (int) -> None):
    a(x)

f:
    print(it)

f(2):
    print(it)
```

## earlier required parameters still need arguments

```by
def g(x: int, a: (int) -> None):
    a(x)

g:  # error: [missing-argument]
    print(it)
```

## a non-callable target is an error

```by
x = 5

x:  # error: [call-non-callable]
    print(it)
```

## the trailing block counts toward the callee's arity

When the call already supplies every parameter, the appended block overfills the last parameter.

```by
def f(x: int, a: (int) -> None):
    a(x)

# error: [parameter-already-assigned]
f(1, lambda (n: int) -> None: print(n)):
    print(it)
```

## unknown callees degrade gracefully

The block is appended positionally and `it` is untyped.

```by
# error: [unresolved-import]
from nowhere import f

f(2):
    reveal_type(it)  # revealed: Unknown
```

## method callees

```by
class Runner:
    def run(self, a: (int) -> None):
        a(1)

Runner().run:
    reveal_type(it)  # revealed: int
```

## callable-typed values

A value of callable type can take a trailing block too; its last parameter's callable type gives
`it` its type.

```by
def call_with(consumer: ((int) -> None) -> None):
    consumer:
        reveal_type(it)  # revealed: int
```

## nested blocks

Each block's `it` is the innermost one.

```by
def f(a: (int) -> None):
    a(1)

f:
    print(it)
    f:
        reveal_type(it)  # revealed: int
```

## a `once` block assignment writes through to the enclosing scope

A `once` block runs exactly once, inline at its call site, so assigning to a name bound in an
enclosing scope updates that binding definitely — `reveal_type` after the block reflects the block's
value (the lowering inserts the matching `nonlocal` / `global`).

```by
from typing_extensions import reveal_type

def run(once fn: () -> None):
    fn()

def main():
    a: int = 1
    run:
        a = 2
    reveal_type(a)  # revealed: 2
```

## a non-`once` block's write unions with the prior value

A non-`once` block may run any number of times, including zero, so even an unconditional write
cannot shadow the prior value — the two union.

```by
from typing_extensions import reveal_type

def run(fn: () -> None):
    fn()

def main():
    a: int = 1
    run:
        a = 2
    reveal_type(a)  # revealed: 1 | 2
```

## a keyword-only `once` callback is recognised

The block binds the callee's last declared parameter even when it is keyword-only, so the `once`
marker there is still honoured — the write narrows definitely rather than unioning.

```by
from typing_extensions import reveal_type

def run(items: list[int], *, once fn: (int) -> None):
    fn(items[0])

def main():
    a: int = 1
    run([1]):
        a = 2
    reveal_type(a)  # revealed: 2
```

## a `return` in a keyword-only `once` block is allowed

Because the keyword-only callback is `once`, its block runs exactly once, so a `return` targeting
the enclosing function is permitted — no `trailing-lambda-control-flow`.

```by
def run(items: list[int], *, once fn: (int) -> None):
    fn(items[0])

def find(items: list[int]) -> int:
    run(items):
        return it
    return -1
```

## an imported `once` callee narrows conservatively

The `once`-ness driving write-back narrowing is resolved syntactically while the semantic index is
built, before imports are followed, so an *imported* `once` callee reads as non-`once` here and the
write unions. This is sound (the union is wider); the runtime lowering, which runs after inference,
still honours the marker.

`callee.by`:

```by
def run(once fn: () -> None):
    fn()
```

`main.by`:

```by
from typing_extensions import reveal_type
from callee import run

def main():
    a: int = 1
    run:
        a = 2
    reveal_type(a)  # revealed: 1 | 2
```

## a module-level binding is captured the same way

```by
from typing_extensions import reveal_type

def run(once fn: () -> None):
    fn()

top: int = 1

run:
    top = 2

reveal_type(top)  # revealed: 2
```

## a `once` block's new binding survives the boundary

A `once` block runs exactly once, like a `with` body, so a name it unconditionally binds — one bound
in no enclosing scope — survives as a definite binding afterwards (the lowering makes it a
`nonlocal` / `global` local of the enclosing scope).

```by
from typing_extensions import reveal_type

def run(once fn: () -> None):
    fn()

def main():
    run:
        fresh = 9
    reveal_type(fresh)  # revealed: 9
```

## a non-`once` block's new binding stays a block local

A non-`once` block may not run, so a name it binds is only possibly bound afterwards. That
possibly-unbound survival is not yet modeled, so such a name stays a block local for now.

```by
def run(fn: () -> None):
    fn()

def main():
    run:
        fresh = 9
    print(fresh)  # error: [unresolved-reference]
```

## a conditional `once` block assignment unions with the prior value

The `once` block runs once, but the assignment is conditional — so on the branch that skips it, `a`
keeps its prior value, giving a union rather than a definite narrowing.

```by
from typing_extensions import reveal_type

def run(once fn: () -> None):
    fn()

def cond() -> bool:
    return True

def main():
    a: int = 1
    run:
        if cond():
            a = 2
    reveal_type(a)  # revealed: 1 | 2
```

## a definite assignment on every branch of a `once` block shadows the prior

When a `once` block rebinds the name on every branch (including a final `else`), the prior value
cannot survive, so it is dropped.

```by
from typing_extensions import reveal_type

def run(once fn: () -> None):
    fn()

def cond() -> bool:
    return True

def main():
    a: int = 1
    run:
        if cond():
            a = 2
        else:
            a = 3
    reveal_type(a)  # revealed: 2 | 3
```

## a binding after a `once` block wins

```by
from typing_extensions import reveal_type

def run(once fn: () -> None):
    fn()

def main():
    a: int = 1
    run:
        a = 2
    a = 3
    reveal_type(a)  # revealed: 3
```

## a block assignment completes an enclosing `let`

A `let` declares a name without binding it. A trailing-lambda block runs inline, so an assignment
there fills the declaration in — the value flows out after the block, with no spurious
`Final`-without-value or unresolved-reference.

```by
from typing_extensions import reveal_type

def run(once fn: () -> None):
    fn()

def main():
    let a: int
    run:
        a = 1
    reveal_type(a)  # revealed: 1
```

## a non-`once` block cannot assign an enclosing `let`

A non-`once` block runs an unknown number of times, so binding an enclosing `let` there could assign
it more than once — which its `Final` declaration forbids.

```by
def run(fn: () -> None):
    fn()

def main():
    let a: int
    run:
        # error: [invalid-assignment] "`a` is `Final`, so a non-`once` trailing-lambda block cannot assign it"
        a = 1
```

`final` is caught the same way.

```by
def run(fn: () -> None):
    fn()

def main():
    final b: int
    run:
        b = 2  # error: [invalid-assignment]
```

A nearer, non-`final` binding shadows a farther `let`, so it is the one written — no error.

```by
from typing_extensions import reveal_type

let top: int = 1

def run(fn: () -> None):
    fn()

def main():
    top = 5
    run:
        top = 2
    reveal_type(top)  # revealed: 5 | 2
```

## a non-`once` block bans `return`

A non-`once` block is an ordinary closure — the callee may run it any number of times — so `return`
may not leave it for the enclosing scope.

```by
def each(items: list[int], fn: (int) -> None):
    for i in items:
        fn(i)

def find(items: list[int]) -> int:
    each(items):
        return it  # error: [trailing-lambda-control-flow]
    return -1
```

## a `break` leaving a block is outside its loop

Because the block is its own function scope, a `break` / `continue` that would leave it is already a
`break`-outside-loop error — one inside a loop the block itself owns is fine.

```by
def each(items: list[int], fn: (int) -> None):
    for i in items:
        fn(i)

def outer(rows: list[list[int]]):
    for row in rows:
        each(row):
            break  # error: [invalid-syntax] "`break` outside loop"

def inner(items: list[int]):
    each(items):
        for x in range(3):
            break
```

## a `once` block permits non-local control flow

A `once` block runs exactly once, like a `with` body, so `return` is allowed.

```by
def each(items: list[int], once fn: (int) -> None):
    fn(items[0])

def find(items: list[int]) -> int:
    each(items):
        return it
    return -1
```

## a `once` block that always returns satisfies the enclosing function

Because a `once` block definitely runs, one that always returns makes the enclosing function return
through it — no fallthrough is needed, and there is no false "implicitly returns `None`".

```by
def each(items: list[int], once fn: (int) -> None):
    fn(items[0])

def find(items: list[int]) -> int:
    each(items):
        return it
```

## a non-`once` block does not satisfy the return

A non-`once` block may not run, so it cannot make the enclosing function return — but its `return`
is banned anyway.

```by
def each(items: list[int], fn: (int) -> None):
    fn(items[0])

def find(items: list[int]) -> int:  # error: [invalid-return-type]
    each(items):
        return it  # error: [trailing-lambda-control-flow]
```

## not valid in `.py` files

The shape parses exactly as it does in upstream python — an annotated assignment missing its
annotation — so `.py` diagnostics are unchanged.

```py
def f(a): ...

f:  # error: [invalid-syntax]
# error: [invalid-syntax] "Unexpected indentation"
# error: [unresolved-reference] "Name `it` used when not defined"
# error: [invalid-syntax] "Expected a statement"
    print(it)
```
