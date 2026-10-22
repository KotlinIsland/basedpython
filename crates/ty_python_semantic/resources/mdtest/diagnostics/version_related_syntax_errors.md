# Version-related syntax error diagnostics

## `match` statement

The `match` statement was introduced in Python 3.10.

### Before 3.10

<!-- snapshot-diagnostics -->

We should emit a syntax error before 3.10.

```toml
[environment]
python-version = "3.9"
```

```py
match 2:  # error: 1 [invalid-syntax] "Cannot use `match` statement on Python 3.9 (syntax was added in Python 3.10)"
    case 1:
        print("it's one")
```

### After 3.10

On or after 3.10, no error should be reported.

```toml
[environment]
python-version = "3.10"
```

```py
match 2:
    case 1:
        print("it's one")
```

## In a basedpython file

A basedpython file is transpiled to python for the version it targets. Syntax the transpiler lowers
is fine at any version, while syntax it writes into the python as it stands has to be syntax that
version parses.

### Syntax the python keeps

A `/` is written into the python as it stands, so a target before 3.8 cannot run it:

```toml
[environment]
python-version = "3.7"
```

```by
def f(a: int, /) -> None:  # error: [invalid-syntax] "Cannot use positional-only parameter separator on Python 3.7 (syntax was added in Python 3.8)"
    pass
```

### Syntax the transpiler lowers

Type parameters are lowered to `TypeVar` declarations, and a `type` statement to a `TypeAliasType`:

```toml
[environment]
python-version = "3.9"
```

```by
def first[T](items: list[T]) -> T:
    return items[0]

type Pair = tuple[int, int]

assert first([1, 2]) == 1
```

### A custom string tag

A string tag's template is written as a t-string, which python only has from 3.14, but the template
is lowered for any target:

```toml
[environment]
python-version = "3.10"
```

```by
def tagged(template: object) -> object:
    return template

greeting = tagged"hello {1 + 1}"
```

### A bracketless `except`

The types of an `except` clause that leaves out their parentheses are written as the parenthesized
tuple they are, which every version parses:

```toml
[environment]
python-version = "3.13"
```

```by
def parse(text: str) -> int:
    try:
        return int(text)
    except ValueError, TypeError:
        return 0

assert parse("x") == 0
```

### A template literal type

An f-string in a type expression is a template literal type, which is written as the `str` or
`Literal[...]` it means, so none of the f-string's own syntax reaches the python. It may reuse its
quote inside a replacement field before 3.12:

```toml
[environment]
python-version = "3.11"
```

```by
def greet(greeting: f"{"hi" | "hello"} {str}") -> str:
    return greeting

assert greet("hi there") == "hi there"
```

### An f-string the python keeps

An f-string value is written into the python as it stands, so it reuses its quote only from 3.12:

```toml
[environment]
python-version = "3.11"
```

```by
name = "world"
greeting = f"{"hello"} {name}"  # error: [invalid-syntax] "Cannot reuse outer quote character in f-strings on Python 3.11 (syntax was added in Python 3.12)"
```

### A `match` statement

A `match` statement is lowered to a chain of `if` statements whose tests bind what the patterns
capture, which takes the assignment expressions of python 3.8:

```toml
[environment]
python-version = "3.8"
```

```by
def describe(x: int) -> str:
    match x:
        case 1:
            return "one"
        case _:
            return "other"

assert describe(1) == "one"
```

### A `match` statement before 3.8

Before 3.8, the `match` statement is left as it is:

```toml
[environment]
python-version = "3.7"
```

```by
def describe(x: int) -> str:
    match x:  # error: [invalid-syntax] "Cannot use `match` statement on Python 3.7 (syntax was added in Python 3.10)"
        case 1:
            return "one"
        case _:
            return "other"
```

### A destructuring pattern

A destructuring pattern — `let P := v`, an `if let` clause, or a pattern written as a `for` or
`with` target or as a parameter — is written as a `match` statement, but is not lowered where a
`match` statement is. It needs python 3.10 whatever the target:

```toml
[environment]
python-version = "3.9"
```

```by
def total(pair: tuple[int, int] | None) -> int:
    # error: [invalid-syntax] "Cannot use a destructuring pattern, which is written as a `match` statement, on Python 3.9 (syntax was added in Python 3.10)"
    if let (a, b) := pair:
        return a + b
    return 0

def first(pair: tuple[int, int]) -> int:
    # error: [invalid-syntax] "Cannot use a destructuring pattern, which is written as a `match` statement, on Python 3.9 (syntax was added in Python 3.10)"
    let (a, _) := pair
    return a

class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int) -> None:
        self.x = x
        self.y = y

def sum_all(points: list[Point]) -> int:
    n = 0
    # error: [invalid-syntax] "Cannot use a destructuring pattern, which is written as a `match` statement, on Python 3.9 (syntax was added in Python 3.10)"
    for Point(x, y) in points:
        n += x + y
    return n

# error: [invalid-syntax] "Cannot use a destructuring pattern, which is written as a `match` statement, on Python 3.9 (syntax was added in Python 3.10)"
def norm(Point(x, y): Point) -> int:
    return x + y
```
