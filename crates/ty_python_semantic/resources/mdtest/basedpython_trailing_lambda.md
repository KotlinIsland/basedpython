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

## the callback may not take more than the block binds

A block binds one argument, as `it`. A callback that takes more would be called with arguments the
block has no parameter for, and that the body cannot name.

```by
def f(a: (int, str) -> None):
    a(1, "two")

f:  # error: [trailing-lambda-parameters] "a trailing-lambda block binds one argument, but this callback takes 2"
    print(it)
```

A keyword-only parameter counts too — the block declares no name for it either.

```by
def g(a: (int, *, flag: bool) -> None):
    a(1, flag=True)

g:  # error: [trailing-lambda-parameters] "a trailing-lambda block binds one argument, but this callback takes 2"
    print(it)
```

A variadic parameter stands for any number of arguments, so a block can never fill it.

```by
def h(a: (*: int) -> None):
    a(1)

h:  # error: [trailing-lambda-parameters] "a trailing-lambda block binds one argument, so it cannot fill a callback with a variadic parameter"
    print(it)
```

The gradual form is the deliberately unchecked one, and is left alone.

```by
def gradual(a: (...) -> None):
    a(1)

gradual:
    print(it)
```

The receiver of an [implicit receiver](basedpython_implicit_receiver.md) callback is bound by the
block itself, so it does not count against the one argument `it` binds.

```by
def against(a: str.(int) -> None):
    a("a", 1)

against:
    reveal_type(self)  # revealed: str
    reveal_type(it)  # revealed: int
```

## a callback that passes nothing never fills `it`

A callback taking no argument is invoked as `a()`. The block still has its `it` parameter — the
lambda the lowering writes always declares one — but nothing ever fills it, so reading it reads the
`None` default rather than a value the call sent.

```by
def f(a: () -> None):
    a()

f:
    # error: [trailing-lambda-parameters] "this block's callback passes no argument, so `it` is never given a value"
    print(it)
```

## an outer `it` is shadowed either way

The block's parameter is what `it` means inside the block, whatever the callback passes, exactly as
it is at runtime — so a name outside is never what the body reads.

```by
it = 5

def f(a: () -> None):
    a()

f:
    # error: [trailing-lambda-parameters] "this block's callback passes no argument, so `it` is never given a value"
    reveal_type(it)  # revealed: Unknown
```

## a block that never reads `it` is fine

```by
def f(a: () -> None):
    a()

f:
    print("hi")
```

## a block may bind `it` itself

A body that writes the name first is reading its own local, not the parameter the call never filled.

```by
def f(a: () -> None):
    a()

f:
    it = 1
    reveal_type(it)  # revealed: 1
```

## an imported callee is read the same way

The semantic index binds the block's `it` before anything resolves across modules, so it cannot
decide this itself — the callee's type does, and an import is no exception.

`callees.by`:

```by
def nothing(fn: () -> None) -> None:
    fn()

def one(fn: (int) -> None) -> None:
    fn(1)
```

```by
from callees import nothing, one

nothing:
    # error: [trailing-lambda-parameters] "this block's callback passes no argument, so `it` is never given a value"
    print(it)

one:
    reveal_type(it)  # revealed: int
```

## a receiver callback with no argument of its own

The receiver is bound implicitly, so `it` is the argument *after* it — which this callback does not
have.

```by
def against(fn: str.() -> None):
    "a".fn()

against:
    print(upper())

against:
    # error: [trailing-lambda-parameters] "this block's callback passes no argument, so `it` is never given a value"
    print(upper(), it)
```

## a callback whose shape cannot be read still declares `it`

The block is filled by whatever the callee passes, so a callback this cannot see through — a gradual
`(...)`, an imported callee, a `Callable[...]` spelling — keeps `it`, untyped.

```by
def f(a: (...) -> None):
    a(1)

f:
    reveal_type(it)  # revealed: Unknown
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

## the block's parameter has to be callable

A block fills the callee's last parameter, so that parameter has to be able to hold one. Binding it
to a parameter that is not callable drops the body into an ordinary argument, where nothing the
author wrote in the block ever runs.

```by
def br(src: str? = None, extra: dict[str, str]? = None) -> None: ...

# error: [invalid-argument-type] "Expected `dict[str, str] | None`, found `(...) -> Unknown`"
br:
    print("this block has nowhere to go")
```

## a parameter that accepts a callable takes the block

The block's own shape stays gradual: its `it` parameter is typed from the callback, and its return
is checked separately, so neither is re-checked here.

```by
def each(fn: (int) -> None) -> None:
    fn(1)

def anything(fn: object) -> None: ...

each:
    print(it)

anything:
    print("a callable is an object")
```

## a block whose body awaits is a coroutine function

The block is a function of its own, so `await` in it is a statement about the block. The callback it
fills is declared to return an awaitable, and the call the block hangs off is awaited by the caller.

```by
async def query(sql: str) -> int:
    return 1

async def scope(name: str, once block: () -> Awaitable[None]):
    await block()

async def main() -> None:
    await scope("db"):
        await query("select 1")
```

## a callback declared to return `None` cannot hold an async block

```by
def hold(once block: () -> None):
    block()

async def read() -> int:
    return 1

async def main() -> None:
    # error: [trailing-lambda-return-type]
    hold():
        await read()
```

## not valid in `.py` files

The shape parses exactly as it does in upstream python — an annotated assignment missing its
annotation — so `.py` diagnostics are unchanged.

```py
def f(a): ...

f:  # error: [invalid-syntax]
# error: [invalid-syntax] "Unexpected indentation"
# error: [unresolved-reference] "Name `it` used when not defined"
    print(it)
```

## a block can be a statement's value

A block written as an assignment's value binds the call the block stands for, not the callee.

```by
def f(x: int, a: (int) -> None) -> str:
    a(x)
    return "done"

result = f(2):
    reveal_type(it)  # revealed: int

reveal_type(result)  # revealed: str
```

## a bare callee takes its value from the same call

```by
def g(a: (int) -> None) -> int:
    a(1)
    return 2

n = g:
    print(it)

reveal_type(n)  # revealed: int
```

## an annotated assignment takes one too

The declared type is checked against the call's, as for any other value.

```by
def f(a: (int) -> None) -> str:
    a(1)
    return "done"

s: str = f:
    print(it)

reveal_type(s)  # revealed: str

# error: [invalid-assignment] "Object of type `str` is not assignable to `int`"
bad: int = f:
    print(it)
```

## a declaration takes one too

A declaration binds the call's return like any other value. `let` still declares the name `Final`
around the block; `var` does not.

```by
def f(a: (int) -> None) -> str:
    a(1)
    return "done"

let declared = f:
    print(it)

var mutable = f:
    print(it)

reveal_type(declared)  # revealed: str
reveal_type(mutable)  # revealed: str

mutable = "other"

# error: [invalid-assignment] "Reassignment of `Final` symbol `declared` is not allowed"
declared = "other"
```

## a `return` takes one too

The returned value is an expression like any other, so a block supplies it. The `def` the block
lowers to is emitted before the `return`, so nothing depends on where in the suite it is written.

```by
def f(a: (int) -> None) -> str:
    a(1)
    return "done"

def build() -> str:
    return f:
        print(it)

reveal_type(build())  # revealed: str
```

The return type is checked against the block's call, not against the block.

```by
def f(a: (int) -> None) -> str:
    a(1)
    return "done"

def build() -> int:
    # error: [invalid-return-type]
    return f:
        print(it)
```

## the call is still checked as a call

```by
def f(x: int, a: (int) -> None) -> str:
    a(x)
    return "done"

# error: [missing-argument] "No argument provided for required parameter `x`"
result = f:
    print(it)
```

## a block's value cannot bind a chain of targets

The block's value is inferred with the definition its target makes, and only one definition can own
it — so the target is a single name.

```by
def f(a: (int) -> None) -> int:
    a(1)
    return 1

# error: [invalid-syntax] "a trailing lambda block's value binds a single name"
# error: [invalid-syntax] "Expected a statement"
x = y = f:
    # error: [invalid-syntax] "Unexpected indentation"
    # error: [unresolved-reference] "Name `it` used when not defined"
    print(it)
```

## a block's value cannot be unpacked

```by
def f(a: (int) -> None) -> tuple[int, int]:
    a(1)
    return (1, 2)

# error: [invalid-syntax] "a trailing lambda block's value binds a single name"
# error: [invalid-syntax] "Expected a statement"
# error: [not-iterable] "Object of type `def f(a: (int, /) -> None) -> (int, int)` is not iterable"
p, q = f:
    # error: [invalid-syntax] "Unexpected indentation"
    # error: [unresolved-reference] "Name `it` used when not defined"
    print(it)
```
