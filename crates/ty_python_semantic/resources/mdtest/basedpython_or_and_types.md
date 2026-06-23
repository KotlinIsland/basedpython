# basedpython: `or` / `and` type operators

basedpython accepts the keywords `or` and `and` in type positions as spellings of union and
intersection — `A or B` is `A | B`, `A and B` is `A & B`.

```toml
[environment]
python-version = "3.12"
```

## `or` is union

```by
def f(x: int or str) -> None:
    reveal_type(x)  # revealed: int | str
```

## `and` is intersection

```by
class P: ...
class Q: ...

def f(x: P and Q) -> None:
    reveal_type(x)  # revealed: P & Q
```

## n-ary chains flatten

```by
class A: ...
class B: ...
class C: ...

def f(x: A and B and C, y: int or str or None) -> None:
    reveal_type(x)  # revealed: A & B & C
    reveal_type(y)  # revealed: int | str | None
```

## `and` binds tighter than `or`

matching python's boolean precedence, and `&` over `|`:

```by
class A: ...
class B: ...
class C: ...

def f(x: A and B or C) -> None:
    reveal_type(x)  # revealed: (A & B) | C
```

## parentheses compose

```by
class A: ...
class B: ...
class C: ...

def f(x: (A or B) and C) -> None:
    reveal_type(x)  # revealed: (A & C) | (B & C)
```

## keyword and symbolic forms mix

```by
class A: ...
class B: ...
class C: ...

def f(x: A & B and C, y: int | str or None) -> None:
    reveal_type(x)  # revealed: A & B & C
    reveal_type(y)  # revealed: int | str | None
```

## nested in a generic

```by
class HasA:
    a: int

class HasB:
    b: str

def f(items: list[HasA and HasB], names: list[int or str]) -> None:
    if items and names:
        reveal_type(items[0].a)  # revealed: int
        reveal_type(items[0].b)  # revealed: str
        reveal_type(names[0])  # revealed: int | str
```

## intersection of attribute presence

```by
class HasA:
    a: int

class HasB:
    b: str

def f(x: HasA and HasB) -> tuple[int, str]:
    return (x.a, x.b)
```

## value positions keep boolean semantics

`or` / `and` outside type positions are ordinary boolean operators:

```by
def f(a: int, b: int) -> None:
    reveal_type(bool(a) or bool(b))  # revealed: bool
    x = True and False
    reveal_type(x)  # revealed: False
```

## still rejected in python files

```py
def f(
    x: int or str,  # error: [invalid-type-form] "Boolean operations are not allowed in parameter annotations"
    y: int and str,  # error: [invalid-type-form] "Boolean operations are not allowed in parameter annotations"
) -> None: ...
```
