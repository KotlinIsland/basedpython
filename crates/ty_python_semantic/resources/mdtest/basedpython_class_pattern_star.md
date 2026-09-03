# Starred wildcards in class patterns

A class pattern's positions are the names its class lists in `__match_args__`. `case A(x, *_, y)`
uses `*_` for the run of them the pattern does not name, so `y` reads the *last* of them however
many there turn out to be.

## A subpattern after the star reads the last position

```by
class Line:
    __match_args__ = ("start", "mid", "stop", "end")
    start: int = 0
    mid: str = ""
    stop: float = 0.0
    end: bool = False

def f(line: Line):
    match line:
        case Line(a, *_, b):
            reveal_type(a)  # revealed: int
            reveal_type(b)  # revealed: bool
```

## Several subpatterns after the star keep their order

They land on the last positions in the order they are written.

```by
class Line:
    __match_args__ = ("start", "mid", "stop", "end")
    start: int = 0
    mid: str = ""
    stop: float = 0.0
    end: bool = False

def f(line: Line):
    match line:
        case Line(*_, a, b):
            reveal_type(a)  # revealed: float
            reveal_type(b)  # revealed: bool
```

## A star written last claims no position

Python already lets a class pattern name fewer positions than the class has, so a trailing `*_`
matches exactly what leaving it out matches.

```by
class Line:
    __match_args__ = ("start", "mid", "stop", "end")
    start: int = 0
    mid: str = ""
    stop: float = 0.0
    end: bool = False

def f(line: Line):
    match line:
        case Line(a, *_):
            reveal_type(a)  # revealed: int
```

## A lone star matches any instance of the class

```by
class Line:
    __match_args__ = ("start", "mid", "stop", "end")
    start: int = 0

def f(value: Line | int):
    match value:
        case Line(*_):
            reveal_type(value)  # revealed: Line
```

## Keyword subpatterns read the names they spell

The star moves the positions around it; a keyword names its attribute outright and is unaffected.

```by
class Line:
    __match_args__ = ("start", "mid", "stop", "end")
    start: int = 0
    mid: str = ""
    stop: float = 0.0
    end: bool = False

def f(line: Line):
    match line:
        case Line(a, *_, mid=m):
            reveal_type(a)  # revealed: int
            reveal_type(m)  # revealed: str
```

## The star sits at whatever depth the pattern does

```by
class Line:
    __match_args__ = ("start", "mid", "stop", "end")
    start: int = 0
    mid: str = ""
    stop: float = 0.0
    end: bool = False

class Wrap:
    __match_args__ = ("inner",)
    inner: Line = Line()

def f(wrap: Wrap):
    match wrap:
        case Wrap(Line(a, *_, b)):
            reveal_type(a)  # revealed: int
            reveal_type(b)  # revealed: bool
```

## A star filling no positions still places what follows it

`Pair` has exactly the two positions the pattern names, so the star stands for nothing.

```by
class Pair:
    __match_args__ = ("first", "second")
    first: int = 0
    second: str = ""

def f(pair: Pair):
    match pair:
        case Pair(a, *_, b):
            reveal_type(a)  # revealed: int
            reveal_type(b)  # revealed: str
```

## Built-in classes match themselves

A class python matches against itself has one position, which is the subject, so a star around it
still lands there.

```by
def f(value: int):
    match value:
        case int(*_, n):
            reveal_type(n)  # revealed: int
```

## The pattern is exhaustive when the positions it names are

The star does not weaken the match: the positions it stands for are the ones no subpattern had to
succeed against.

```by
class Pair:
    __match_args__ = ("first", "second")
    first: int = 0
    second: str = ""

def f(pair: Pair) -> str:
    match pair:
        case Pair(_, *_, b):
            return b
```

## The star still binds nothing

`*_` is the wildcard, so it introduces no name of its own.

```by
class Line:
    __match_args__ = ("start", "mid", "stop", "end")
    start: int = 0
    end: bool = False

def f(line: Line):
    match line:
        case Line(a, *_, b):
            # error: [unresolved-reference]
            reveal_type(_)  # revealed: Unknown
```

## A star has to be the wildcard

There is no sequence behind a class pattern's positions — only the names `__match_args__` lists — so
a star there has nothing to capture.

```by
class Pair:
    __match_args__ = ("first", "second")

def f(pair: Pair):
    match pair:
        # error: [invalid-syntax] "A starred subpattern in a class pattern must be the wildcard `*_`"
        case Pair(a, *rest):
            pass
```

## Only one star fits in a pattern

Two would leave the positions between them unplaceable.

```by
class Pair:
    __match_args__ = ("first", "second")

def f(pair: Pair):
    match pair:
        # error: [invalid-syntax] "Only one starred subpattern is allowed in a class pattern"
        case Pair(*_, a, *_):
            pass
```

## The star is a positional subpattern, so it cannot follow a keyword one

```by
class Pair:
    __match_args__ = ("first", "second")

def f(pair: Pair):
    match pair:
        # error: [invalid-syntax] "Positional patterns cannot follow keyword patterns"
        # error: [invalid-syntax] "Positional patterns cannot follow keyword patterns"
        case Pair(first=a, *_, b):
            pass
```

## Python does not have this form

```py
class Pair:
    __match_args__ = ("first", "second")

def f(pair: Pair):
    match pair:
        # error: [invalid-syntax] "Star pattern cannot be used here"
        case Pair(a, *_, b):
            pass
```
