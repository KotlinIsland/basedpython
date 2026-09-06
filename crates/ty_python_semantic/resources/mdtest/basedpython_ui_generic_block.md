# basedpython-ui: a block's `it` on a generic callee is typed from the call

A generic free function's callback parameter mentions the function's own type variables, so what a
block's `it` (and receiver) are is only known once the call's written arguments have solved them.
The block is typed from that solution — as it already is for a bound method, whose receiver carries
its specialization — rather than from the unsolved `T`. A compose-style `each(items):` helper is
exactly this shape.

## `it` takes the solved type

```by
def each[T](items: tuple[T, ...], local block: (T) -> None):
    for item in items:
        block(item)

def use(names: tuple[str, ...], counts: tuple[int, ...]):
    each(names):
        reveal_type(it)  # revealed: str

    each(counts):
        reveal_type(it)  # revealed: int
```

## the solution is as precise as the argument

A literal display solves `T` to its literal elements.

```by
def each[T](items: tuple[T, ...], local block: (T) -> None):
    for item in items:
        block(item)

each(("a", "b")):
    reveal_type(it)  # revealed: "a" | "b"
```

## a keyword argument solves it too

```by
def each[T](items: tuple[T, ...], local block: (T) -> None):
    for item in items:
        block(item)

def use(names: tuple[str, ...]):
    each(items=names):
        reveal_type(it)  # revealed: str
```

## the receiver takes the solved type

```by
def with_each[T](items: tuple[T, ...], block: T.() -> None):
    for item in items:
        item.block()

def use(names: tuple[str, ...]):
    with_each(names):
        reveal_type(upper())  # revealed: str
```

## the solved `it` fills a `context` parameter

```by
def each[T](items: tuple[T, ...], local block: (T) -> None):
    for item in items:
        block(item)

def show(context label: str): ...

each(("a", "b")):
    show()
```

## a bound method still specializes from its receiver

```by
class Items[T]:
    def each(self, local block: (T) -> None): ...

def use(items: Items[str]):
    items.each:
        reveal_type(it)  # revealed: str
```

## an unpacked argument leaves the callee unsolved

The block is not solved from a call whose arguments cannot be bound statically, so `it` keeps the
declared type variable.

```by
def each[T](items: tuple[T, ...], local block: (T) -> None):
    for item in items:
        block(item)

def use(args: tuple[tuple[str, ...]]):
    each(*args):
        reveal_type(it)  # revealed: T@each
```
