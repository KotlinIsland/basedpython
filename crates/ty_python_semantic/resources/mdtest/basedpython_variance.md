# basedpython: use-site variance with `out` and `in`

basedpython supports use-site variance keywords on subscript elements:

- `Container[out T]` — covariant read-only view. Reading returns `T`; writing is rejected.
- `Container[in T]` — contravariant write-only projection. Accepts writes of `T`; reads project to
    `object`.
- `Container[in out T]` — invariant read-write view. Equivalent to plain `Container[T]` for
    read/write purposes.

The outer container's identity is preserved — `Container[out T]` is still a `Container`, with member
access projected according to the use-site variance, just like Kotlin's `Container<out T>` or Java's
`Container<? extends T>`.

## generic attribute write under `out`

```by
class Box[T]:
    value: T

def f(box: Box[out object]):
    # error: [invalid-assignment] "Cannot assign value of type `"asdf"` to attribute `value` on covariantly-projected object of type `Box[out object]`"
    box.value = "asdf"
```

## covariant `out`

```by
def f(data: list[out int]):
    reveal_type(data[0])  # revealed: int
    # error: [invalid-assignment] "Invalid subscript assignment with key of type `0` and value of type `1` on object of type `list[out int]`"
    data[0] = 1
```

The annotation also reveals the variance projection directly:

```by
def f(data: list[out int]):
    reveal_type(data)  # revealed: list[out int]
```

## contravariant `in`

`in T` allows writing `T` and projects reads through to `object`.

```by
def f(data: list[in int]):
    data[0] = 1  # ok: int accepted
    # error: [invalid-assignment]
    data[0] = "bad"
```

Reads return `object`, so a narrower-typed target rejects the read:

```by
def f(data: list[in int]):
    reveal_type(data[0])  # revealed: object
    b: object = data[0]  # ok
    # error: [invalid-assignment]
    a: int = data[0]
```

## invariant `in out`

`in out T` reads and writes as `T`, like the plain subscript form:

```by
def f(data: list[in out int]):
    reveal_type(data[0])  # revealed: int
    data[0] = 1  # ok
    # error: [invalid-assignment]
    data[0] = "bad"
```

## complex inner types

The inner type expression can be arbitrarily complex:

```by
def _(a: list[out int | str]):
    reveal_type(a[0])  # revealed: int | str
```

The same holds under `in` — writes accept the whole union, reads still project to `object`:

```by
def _(a: list[in int | str]):
    reveal_type(a)  # revealed: list[in int | str]
    a[0] = 1  # ok
    a[0] = "ok"  # ok
    # error: [invalid-assignment]
    a[0] = b"bad"
    reveal_type(a[0])  # revealed: object
```

and under `in out`, which reads and writes the union like the plain form:

```by
def _(a: list[in out int | str]):
    a[0] = 1  # ok
    # error: [invalid-assignment]
    a[0] = b"bad"
    reveal_type(a[0])  # revealed: int | str
```

## variance on a slice element other than the first

Every element of the subscript carries its own variance, not just the first one:

```by
def _(d: dict[str, in int]):
    reveal_type(d)  # revealed: dict[str, in int]
    d["a"] = 1  # ok
    # error: [invalid-assignment]
    d["a"] = "bad"
    reveal_type(d["a"])  # revealed: object
```

## mixed variance across the parameters

```by
def _(d: dict[out str, in int]):
    reveal_type(d)  # revealed: dict[out str, in int]
    # the key projects covariantly and the value contravariantly
    reveal_type(d.keys())  # revealed: dict_keys[str, object]
```

## mutating methods under `out`

The projection is not limited to subscripts and attributes — it reaches every member. A method that
consumes the element type (like `list.append`) takes it in a contravariant position, which projects
to `Never` under `out`, so no argument can be written:

```by
def f(a: list[out int | str]):
    reveal_type(a.append)  # revealed: bound method list[out int | str].append(object: Never, /)
    # error: [invalid-argument-type] "Argument to bound method `list.append` is incorrect: Expected `Never`, found `"a"`"
    a.append("a")
    # even a value of the element type is rejected — an `out` view writes nothing
    # error: [invalid-argument-type]
    a.append(1)
    # error: [invalid-argument-type]
    a.extend([1, 2])
```

A method that *produces* the element type reads it back covariantly, so it is unaffected:

```by
def f(a: list[out int]):
    reveal_type(a.pop())  # revealed: int
```

## mutating methods under `in`

`in` is the mirror image: writes are accepted at the element type, reads project to `object`.

```by
def f(a: list[in int]):
    a.append(1)  # ok
    # error: [invalid-argument-type]
    a.append("bad")
    reveal_type(a.pop())  # revealed: object
```

Union element types are consumed whole:

```by
def f(a: list[in int | str]):
    reveal_type(a.append)  # revealed: bound method list[in int | str].append(object: int | str, /)
    a.append(1)  # ok
    a.append("ok")  # ok
    # error: [invalid-argument-type] "Argument to bound method `list.append` is incorrect: Expected `int | str`, found `b"bad"`"
    a.append(b"bad")
    reveal_type(a.pop())  # revealed: object
```

## subtyping under projection

Use-site projections promote an invariant generic to covariant or contravariant *at the call site*,
matching Kotlin's `Container<out T>` / `Container<in T>` rules.

`Container[out X]` is a supertype of `Container[Y]` whenever `Y <: X`:

```by
def widening(bools: list[bool]):
    # list[bool] <: list[out int] because bool <: int
    y: list[out int] = bools
```

`Container[in X]` is a supertype of `Container[Y]` whenever `Y :> X`:

```by
def widening_in(objs: list[object]):
    # list[object] <: list[in int] because int <: object
    y: list[in int] = objs
```

A projection is itself a wider set than the concrete form, so narrowing from a projection back to
the concrete form is rejected:

```by
def reject_narrowing(out_ints: list[out int]):
    # error: [invalid-assignment]
    y: list[int] = out_ints
```

`out` and `in` projections describe variance in opposite directions and have no subtyping relation:

```by
def reject_opposite(out_ints: list[out int]):
    # error: [invalid-assignment]
    y: list[in int] = out_ints
```

Two `out` projections relate by the inner type:

```by
def out_to_out(out_bools: list[out bool]):
    # list[out bool] <: list[out int] because bool <: int
    y: list[out int] = out_bools
```

## definition-site variance is independent

`out`/`in`/`in out` on type-parameter declarations is unrelated machinery that controls how each
instantiation specializes the underlying class:

```by
class Box[out T]:
    def get(self) -> T:
        raise NotImplementedError

def _(box: Box[int]):
    reveal_type(box.get())  # revealed: int
```

## `out` as an ordinary variable is not variance

Only `out` immediately followed by a *name* (`out T`) is a variance prefix — two adjacent names are
never valid Python. `out` followed by `[`, `(` or `.` is an ordinary subscript, call or attribute on
a variable named `out`, and must parse as plain Python. `out` is a common variable name; this
regressed on real code (`home-assistant` has `xs[out[0]]`):

```py
def f(xs: list[int], out: tuple[int, int]):
    reveal_type(xs[out[0]])  # revealed: int

def g(out: list[int]):
    reveal_type(out[0])  # revealed: int
```

## a declared `in out` does not widen a parameter the body never writes

`in out T` and a bare `T` are different declarations: the first pins invariance, the second leaves
variance to be inferred from the body. But *widening a literal solution* is not the subtyping
question — it asks whether a write can reach the parameter, so that a later write of a different
type would conflict with the literal the first one happened to have. A class that only takes `T` in
`__init__` has no such write under either spelling, so both keep the literal.

```by
class Bare[T]:
    init(t: T)

class Marked[in out T]:
    init(t: T)

reveal_type(Bare(1))  # revealed: final Bare[1]
reveal_type(Marked(1))  # revealed: final Marked[1]

def bare() -> Bare[1]:
    result = Bare(1)
    return result

def marked() -> Marked[1]:
    result = Marked(1)
    return result
```

A member that really does read *and* write the parameter still widens, under either spelling — the
literal could not survive a second write of a different type.

```by
class BareRW[T]:
    x: T

    init(t: T)

class MarkedRW[in out T]:
    x: T

    init(t: T)

reveal_type(BareRW(1))  # revealed: final BareRW[int]
reveal_type(MarkedRW(1))  # revealed: final MarkedRW[int]
```

Only the literal is unaffected — see below for what the declaration does do.

## a bare `T` infers its variance; `in out T` pins it

These are different declarations, not two spellings of one. A bare `T` follows what the body does
with it, so the same members come out covariant in one class and contravariant in another; `in out`
overrides that and seals the subtyping relation.

```by
class Reads[T]:
    def get(self) -> T:
        raise NotImplementedError

class Sealed[in out T]:
    def get(self) -> T:
        raise NotImplementedError

def f(r: Reads[int], s: Sealed[int]):
    # `Reads`'s variance is inferred from the body, which only produces `T`
    inferred_covariant: Reads[object] = r
    # error: [invalid-assignment]
    declared_invariant: Sealed[object] = s

class Writes[T]:
    def put(self, t: T) -> None: ...

class SealedSink[in out T]:
    def put(self, t: T) -> None: ...

def g(w: Writes[object], s: SealedSink[object]):
    inferred_contravariant: Writes[int] = w
    # error: [invalid-assignment]
    declared_invariant: SealedSink[int] = s
```
