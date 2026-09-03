# basedpython: declaring variables with `let` and `var`

`implicit-declaration` asks that every variable a scope binds be declared once, with `let` for a
binding that never changes and `var` for one that does. It is off by default, because a file written
without the keywords is valid basedpython.

```toml
[environment]
python-version = "3.12"

[rules]
implicit-declaration = "error"
```

## an assignment to an undeclared name is reported

```by
count = 0  # error: [implicit-declaration]
```

## a declared variable may be assigned as often as it likes

```by
var count = 0
count = count + 1
count = 0
```

## `let` declares too

```by
let limit = 10
reveal_type(limit)  # revealed: 10
```

## the declaration may come later in the scope

The rule asks that the scope declare the name, not that the declaration come first — a `var` further
down still says what the name is.

```by
def f(flag: bool):
    if flag:
        total = 1
    var total: int
```

## an annotation is not a declaration keyword

An annotated assignment says what the type is, but not whether the binding may change; `var` is the
same statement with that said.

```by
total: int = 0  # error: [implicit-declaration]
```

## a class body declares fields, not variables

`x: int` in a class body is how a dataclass or a protocol declares a field, so it is left alone.
`class x = 1` is the class-variable form and has a keyword of its own.

```by
data class Point:
    x: int
    y: int = 0

class C:
    class count = 0
```

## an assignment to something other than a name declares nothing

```by
class C:
    var items: list[int]
    var tag: str

    def f(self, other: C):
        other.tag = "x"
        other.items[0] = 1
```

## an augmented assignment is not a declaration

It requires the name to already exist, so it never introduces one.

```by
def f():
    var total = 0
    total += 1
```

## a `for` target, a `with` item and an `except` name are not reported

Each of these binds with a keyword of its own.

```by
def f(values: list[int], path: str):
    for value in values:
        print(value)

    with open(path) as handle:
        print(handle)

    try:
        pass
    except ValueError as error:
        print(error)
```

## a `global` or `nonlocal` name is declared by the scope that owns it

```by
var counter = 0

def bump():
    global counter
    counter = counter + 1

def outer():
    var total = 0

    def inner():
        nonlocal total
        total = 1
```

## unpacking is not reported

A binder that binds several names at once is written `let [a, b] := value`, and a plain unpacking is
left alone.

```by
def f(values: tuple[int, str]):
    first, second = values
    reveal_type(first)  # revealed: int
```

## a stub is left alone

A stub says what a module has, never what it does, so a declaration there needs no keyword.

```byi
count: int
```
