# unique loop bindings

python gives a loop *one* binding for its target, shared by every iteration, so
a closure made inside the body reads whatever that variable holds later — not
what it held when the closure was made:

```by
fns = []
for i in [1, 2, 3]:
    fns.append(lambda: print(i))
for fn in fns:
    fn()
```

in python that prints `3`, `3`, `3`. in basedpython it prints `1`, `2`, `3`:
each iteration gets its own binding, the same change go made in 1.22

a comprehension target has the identical one-cell-per-comprehension behaviour,
and is bound the same way:

```by
fns = [lambda: i for i in [1, 2, 3]]
assert [fn() for fn in fns] == [1, 2, 3]
```

every target of a destructuring loop binds, and nested loops each contribute
their own:

```by
for left, right in pairs:
    fns.append(lambda: (left, right))
```

## what is captured

a closure captures the loop bindings it actually *reads through*. a name it
binds itself is untouched, so the hand-written idiom keeps working unchanged:

```by
for i in items:
    fns.append(lambda i: i)      # `i` is the parameter, not the loop's
    fns.append(lambda i=i: i)    # the python workaround, left alone
    fns.append(lambda: [i for i in inner])   # the inner target shadows
```

shadowing is decided by real name resolution, not by matching text, so an
intervening scope that binds the name also ends the capture

a name a nested function writes through with `nonlocal` or `global` is never
captured — the write has to reach the binding the loop itself holds:

```by
def sum_items() -> int:
    total = 0
    for i in items:
        def bump():
            nonlocal total
            total += i     # `i` is captured, `total` writes through
        bump()
    return total
```

## how it lowers

an expression closure — a `lambda`, a generator expression — is applied to the
values through a wrapper whose parameters shadow them. the closure body keeps
its own source, and closes over the wrapper's fresh parameter cells:

```py
fns.append((lambda i: lambda: print(i))(i))
```

a `def` is a statement and cannot be wrapped that way, so it gets a decorator
that rebuilds the function with fresh cells for the captured names. every other
cell — outer locals, the implicit `__class__` of a zero-argument `super()`, a
[reified type parameter](reified-generics.md) — is carried across untouched, as
are the name, docstring, defaults, annotations and attributes. the decorator is
inserted innermost, below any decorator you wrote, so it always receives the raw
function whose closure it rebuilds:

```py
for i in items:
    @app.get(i)
    @_by_loop_bind(i=i)
    def handler():
        return i
```

a method of a class defined in a loop is a `def` like any other, and is bound
the same way

## what stays python's

- a **`while` loop** has no target, so nothing is bound. the same goes for a
    name merely *assigned* in the body: python resolves those late, which is
    what lets two functions defined in the same body call each other, so
    freezing them would break more than it fixed
- the capture happens **where the closure is written**, not at the end of the
    iteration. rebinding the target later in the same iteration is not seen by a
    closure made before it
- a **`def` inside a module-level loop** is left alone. python compiles a read
    of a module-level name as a global rather than a closure cell, and the
    rebuild has no cell to swap. a `lambda` in the same position *is* bound
    (the wrapper introduces a real cell), and moving the loop into a function —
    a [`main`](main-function.md), say — binds the `def` too. ruff's `B023`
    reports exactly this case in a `.by` file, and stays quiet about the ones
    that are bound
- a function the **parser synthesized** for another construct — a
    [trailing-lambda block](trailing-lambdas.md), a
    [property accessor](properties.md) — is left to the construct that owns it,
    which emits the whole thing itself. for a trailing-lambda block that is the
    right division anyway: whether its closure outlives the iteration depends on
    the callee's [`local` / `once`](local-lifetimes.md) marker, which ty decides
    precisely as `escaping-loop-variable`. a closure written *inside* such a
    body is bound as usual, and `B023` reports a property accessor that reads a
    loop variable, the same way it reports a module-level `def`
- a **generator that publishes a name** outward — one containing a `:=`, which
    binds in the scope around the generator — is left alone. the wrapper would
    carry that binding off with it, and a name arriving where it was written to
    arrive outranks the capture
- the `else` clause of a loop runs once, after the target's final value, so
    closures there are ordinary closures

## turning it off

it is on by default. `--no-unique-loop-bindings` emits python's sharing
instead:

```sh
by run main --no-unique-loop-bindings
```

the flag is accepted by `by run`, `by build` and `by transpile`
