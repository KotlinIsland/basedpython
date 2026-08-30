# basedpython: a class-body declaration every instance shares

A class body runs once, so a value written there is built once. For a scalar that never shows —
writing `instance.count = 1` rebinds on the instance and leaves the class's own value alone — but a
mutable value is reached through rather than rebound, so `instance.seen.add(1)` changes the single
object every instance is looking at.

`shared-mutable-default` reports the `let` and `var` declarations where those two readings come
apart.

```toml
[environment]
python-version = "3.12"
```

## a mutable value on a class-body `var`

```by
class Fight:
    var seen: set[int] = set()  # error: [shared-mutable-default]
```

## a mutable value on a class-body `let`

`let` declares the binding read-only, which says nothing about the object it is bound to: the set is
still one set, and still mutable through every instance.

```by
class Fight:
    let seen: set[int] = set()  # error: [shared-mutable-default]
```

## an immutable value is left alone

A number, a string, a bool and `None` cannot be reached through, so the only way to change one is to
rebind it — and rebinding is per instance.

```by
class Fight:
    var last_contact: int = 0
    var name: str = ""
    var over: bool = False
    var winner: str? = None
```

## the value's own type decides, not the shape it was written in

A computed value is an `int` however it was written, so it is left alone the same way a literal is.
Reading the value's syntax instead would report every scalar that was not written as one.

```by
def measure() -> int:
    return 1

class Step:
    var seq: int = 1 + 1
    var width: int = measure()
```

## nor the declared type, which is wrong in both directions

What is shared is the value, so the value is what is measured. An optional field initialised to
`None` shares a `None`, whatever else the declaration will later hold — and a declaration widened to
`object` would hide a list if the declaration were what counted.

```by
class Point: ...

class Base:
    var where: Point? = None
    var anything: object = []  # error: [shared-mutable-default]
```

## a fixed-length tuple of immutable elements is immutable too

```by
class Board:
    var origin: tuple[int, int] = (0, 0)
```

## a tuple holding a mutable element is not

Nothing can be written through the tuple, but the list inside it is still one list.

```by
class Board:
    var rows: tuple[list[int], list[int]] = ([], [])  # error: [shared-mutable-default]
```

## a user class is assumed mutable

The reading is about what the value *is*, so a constructor call is reported the same way a `[]` is.

```by
class Roster:
    init(let members: list[str])

class Team:
    var roster: Roster = Roster([])  # error: [shared-mutable-default]
```

## `class var` says the sharing is intended

`class var` is the spelling for class-level state, and lowers to `ClassVar`. One object for the
whole class is what it asks for, so it is not reported.

```by
class Registry:
    class var seen: set[int] = set()
```

## `class let` says the same for a read-only one

```by
class Registry:
    class let origin: list[int] = []
```

## declaring the field and assigning it in `init` is the per-instance form

Nothing is built in the class body, so there is nothing to share.

```by
class Fight:
    var seen: set[int]

    init():
        self.seen = set()
```

## a plain python declaration keeps python's meaning

An annotated assignment with no declaration keyword is ordinary python, where a class-body value is
a class attribute and is meant to be. Only the basedpython declaration forms — which read as fields
— are reported.

```by
class Fight:
    seen: set[int] = set()
```
