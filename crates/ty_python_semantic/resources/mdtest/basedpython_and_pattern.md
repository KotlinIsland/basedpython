# basedpython: the `and` pattern

`P and Q` matches a value that both `P` and `Q` match, and binds the captures of both. It is
python's missing counterpart to `|`, and binds tighter than it: `A() and B() | C()` is
`(A() and B()) | C()`.

## Every conjunct binds

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def f(v: object):
    if let Point(x, y) and object() := v:
        reveal_type(x)  # revealed: int
        reveal_type(y)  # revealed: int
```

## A conjunction narrows to what every conjunct matches

```by
class A: ...

class B(A): ...

def f(v: A | int):
    if let A() and B() := v:
        reveal_type(v)  # revealed: B
```

## A conjunction of disjoint patterns matches nothing

```by
def f(v: int | str):
    if let int() and str() := v:
        reveal_type(v)  # revealed: Never
```

## Conjuncts see what the ones before them narrowed

```by
class Base:
    kind: str

class Sub(Base):
    __match_args__ = ("kind",)

def f(v: Base):
    if let Sub() and Sub(kind) := v:
        reveal_type(v)  # revealed: Sub
        reveal_type(kind)  # revealed: str
```

## A conjunct binds each name once

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def f(v: object):
    # error: [invalid-syntax] "multiple assignments to name `x` in pattern"
    # error: [invalid-syntax] "multiple assignments to name `y` in pattern"
    if let Point(x, y) and Point(x, y) := v:
        pass
```

## In a `match` case

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def f(v: object) -> int:
    match v:
        case Point(x, y) and object():
            reveal_type(x)  # revealed: int
            return x + y
        case _:
            return 0
```

## In a destructuring binder

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def f(p: Point):
    let Point(x, y) and object() := p
    reveal_type(x)  # revealed: int
```

## An irrefutable conjunction is irrefutable

Every conjunct has to match, so the conjunction is only irrefutable when all of them are.

```by
class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

def f(p: Point):
    # error: [refutable-destructuring]
    let Point(x, y) and Point(x=0, y=0) := p
```

## An `and` cannot be written inside an alternative

Every alternative of a `|` binds the same names, which a conjunction — matched one conjunct at a
time, against a binder standing in for its position — cannot preserve.

```by
def f(v: object):
    # error: [invalid-syntax] "an `and` pattern cannot be written inside an alternative of a `|` pattern"
    if let int() | (str() and "x") := v:
        pass
```
