# basedpython: no-op annotation modifiers

`override`, `abstract` and the visibility modifiers may prefix an annotated declaration:
`override x: T [= v]`. They carry no type-level meaning, so the declaration means exactly `x: T` —
but the declared type is still a declaration and must be enforced. Contrast `final x: T` (see
`basedpython_final.md`), whose `Final` qualifier does survive.

```toml
[environment]
python-version = "3.12"
```

## the declared type is enforced

```by
override x: int = 1
reveal_type(x)  # revealed: 1

override bad: int = "nope"  # error: [invalid-assignment]
```

## `abstract` declares its type

```by
abstract a: str = "s"
reveal_type(a)  # revealed: "s"

abstract bad_a: str = 1  # error: [invalid-assignment]
```

## a visibility modifier declares its type

```by
private p: int = 1
reveal_type(p)  # revealed: 1

private bad_p: int = "nope"  # error: [invalid-assignment]
```

## a valueless declaration still declares

A valueless `override y: int` declares `y` as `int` — it does not assign the class object `int` to
`y`, which is what stashing the annotation in the value position used to mean.

```by
override y: int

y = 1
reveal_type(y)  # revealed: 1

y = "nope"  # error: [invalid-assignment]
```

## a modifier chain declares its type

```by
override abstract c: int = 1

c = 2
c = "nope"  # error: [invalid-assignment]
```

## a no-op modifier is not `final`

Unlike `final x: T`, these modifiers leave the binding writable.

```by
override m: int = 1
m = 2

abstract n: int = 1
n = 2
```

## an unannotated modifier declares nothing

Without a type there is nothing to declare, so `override x = v` means exactly `x = v` — including
for readers that prefer a declaration over an inference.

`m.by`:

```by
override exported = 1

class C:
    override count = 0
```

```by
from m import exported, C

reveal_type(exported)  # revealed: 1
reveal_type(C.count)  # revealed: int
```

## inside a class body

```by
class C:
    override tag: str = "c"

reveal_type(C.tag)  # revealed: str
C.tag = "x"
C.tag = 1  # error: [invalid-assignment]
```

## a soft keyword can be the name

`type`, `match` and `case` are keywords only where they introduce their own construct, so a
declaration may name one — typeshed's `socket` and `asyncio.trsock` both declare a `type` field.

```by
class C:
    let type: int = 0
    override match: str = "m"
    private case: bytes = b""

def f(c: C) -> None:
    reveal_type(c.type)  # revealed: int
    reveal_type(c.match)  # revealed: str
```

The construct `type` does introduce still wins where its own shape appears.

```by
private type Alias = int

def g(x: Alias) -> None:
    reveal_type(x)  # revealed: int
```
