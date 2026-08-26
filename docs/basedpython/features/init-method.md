# init method shorthand

basedpython lets a class declare its constructor with the bare keyword `init`
instead of `def __init__`. parameters prefixed with `let` are auto-assigned to
`self.<name>` at the top of the method body, so the constructor and the
instance-attribute declarations share a single signature

```by
class Point:
    init(self, let x: int, let y: int)
```

transpiles to:

```python
class Point:
    def __init__(self, x: int, y: int):
        self.x: int = x
        self.y: int = y
```

## scope

`init(...)` is only recognised inside a class body. at module scope
`init(...)` is still an ordinary function call expression. the parser tracks
class nesting so the keyword does not leak into module-level call sites

## bodyless and body forms

both shapes are accepted:

```by
class A:
    init(self, let a: int, b: str)
```

```by
class B:
    init(self, a: int):
        self.b = str(a)
```

a bodyless `init(...)` is filled in with `: ...` when no `let` parameter
produces a body line. body-bearing `init(...)` keeps the user's statements;
`let` self-assignments are prepended ahead of them

## defaults

a parameter default is lowered exactly as it is on a `def`. a default that is
constructed rather than a constant is re-evaluated per call rather than shared
between every instance, and a required parameter may follow a defaulted one:

```by
class Fight:
    init(
        let seen: set[int] = set(),
        let last: int,
    )
```

```python
class Fight:
    def __init__(self, seen: set[int] = _MISSING, last: int = _MISSING):
        if seen is _MISSING:
            seen = set()
        if last is _MISSING:
            raise TypeError("__init__() missing required argument: 'last'")
        self.seen: set[int] = seen
        self.last: int = last
```

## `let` parameter modifier

`let` may appear on any positional, positional-only, keyword-only, `*args`,
or `**kwargs` parameter inside `init`. each `let` parameter emits a
self-assignment using the parameter's annotation (`self.<name>: <ann> = <name>`)
or, if unannotated, a bare assignment (`self.<name> = <name>`)

a non-`let` parameter is just a parameter — no attribute is created for it

the attribute is read-only, exactly as a class-body `let a: int` is: the
constructor binds it and nothing else may write it

```by
class A:
    init(let a: int)

A(1).a = 2  # rejected
```

that is what lets a class be covariant in a type parameter it only stores —
a widened view cannot corrupt an attribute nothing can write to:

```by
class Box[T]:
    init(let t: T)

def _(box: Box[int]):
    widened: Box[object] = box
```

## `var` and visibility modifiers

`var` is the mutable counterpart of `let`; on an `init` parameter it
self-assigns identically, but the attribute stays writable — and a class that
stores one is invariant in its type. a visibility modifier — `private` or `public` — may
precede `let` / `var`. `private` name-mangles the synthesised attribute to
`self.__name`, while the parameter itself keeps its declared name:

```by
class A:
    init(private var a: int)
```

transpiles to:

```python
class A:
    def __init__(self, a: int):
        self.__a: int = a
```

a visibility modifier without `let` / `var` has no attribute to name, and any
other modifier keyword (`final`, `abstract`, …) is meaningless in this
position — both are reported as errors

## implicit `self`

`self` may be omitted from the parameter list. it is implied, so it is
injected into the generated `def __init__` signature and `let` / `var`
attributes and `self` references in the body resolve against it:

```by
class A:
    init(let a: int)
```

transpiles to:

```python
class A:
    def __init__(self, a: int):
        self.a: int = a
```

## why

constructors with `self.x = x; self.y = y` boilerplate are ubiquitous.
collapsing the parameter list and the attribute declarations into one line
removes that duplication without depending on `@dataclass` semantics
