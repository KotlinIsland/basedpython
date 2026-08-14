# refutable unpacking

unpacking a value binds every target or none of them, so a value that yields the wrong number of
elements raises `ValueError` and leaves nothing behind. `refutable-unpacking` reports an unpacking
whose value is not known to have the number of elements the targets ask for

```python
def f() -> tuple[int, ...]:
    return ()

a, b = f()  # error: `tuple[int, ...]` may not have exactly 2 elements
```

`tuple[int, ...]` says what the elements are and nothing about how many there are, so two targets is
a guess. the typing spec accepts it — a variable-length tuple may well hold two — and it is the
guess that this check is about

a length the type carries is checked exactly, by the existing "too many values" and "not enough
values" errors, and is not reported here

```python
def f() -> tuple[int, str]:
    return (1, "a")

a, b = f()  # ok
```

## every iterable, not just tuples

the same reasoning covers a `list`, a `set`, a generator, and any class with an `__iter__` — none of
them put their length in their type

```python
def f(numbers: list[int]):
    a, b = numbers  # error: `list[int]` may not have exactly 2 elements
```

## starred targets

a starred target absorbs whatever is left over, so it only requires the targets around it. those
still have to be there — an empty list has nothing to give to `first`

```python
def f(numbers: list[int]):
    first, *rest = numbers  # error: `list[int]` may not have at least 1 element
    (*everything,) = numbers  # ok — requires nothing
```

a [mixed tuple](tuple-types.md) pins its ends and leaves the middle open, which is exactly what a
starred target wants

```python
def f(values: tuple[int, *tuple[str, ...], bool]):
    first, *middle, last = values  # ok — the two ends are in the type
```

## every binder

a `for` target, a `with` item and a comprehension all unpack the same way, and are all checked

```python
def f(rows: list[list[int]]):
    for a, b in rows:  # error: `list[int]` may not have exactly 2 elements
        ...
```

## splatted arguments

`f(*values)` binds the parameters positionally out of `values`, so a length that does not match is a
`TypeError` for the same reason `a, b = values` is a `ValueError`. arguments written before the
splat count towards the parameters, and a parameter with a default does not have to be reached

```python
def f(a: int, b: int, c: int = 0): ...

def g(values: tuple[int, ...]):
    f(*values)  # error: `tuple[int, ...]` may not have between 2 and 3 elements
    f(1, *values)  # error: `tuple[int, ...]` may not have between 1 and 2 elements
```

an `*args` parameter takes however many elements are left, so a splat only has to reach the
parameters before it

```python
def takes_any(*args: int): ...
def takes_one_then_any(a: int, *args: int): ...

def f(values: tuple[int, ...]):
    takes_any(*values)  # ok — nothing is required and nothing is too much
    takes_one_then_any(*values)  # error: `tuple[int, ...]` may not have at least 1 element
    takes_one_then_any(1, *values)  # ok — `a` is already filled
```

that is what keeps the forwarding idiom quiet: a `ParamSpec` pairs an `*args` argument with an
`*args` parameter

```python
def deco[**P, R](func: Callable[P, R]) -> Callable[P, R]:
    def wrapper(*args: P.args, **kwargs: P.kwargs) -> R:
        return func(*args, **kwargs)  # ok

    return wrapper
```

## what is not reported

a value with no type at all has opted out of checking, along with everything else

```python
from typing import Any

def f(value: Any):
    a, b = value  # ok
```

an `Any` *element* type is a different statement: the length of a `list[Any]` is as unknown as the
length of a `list[int]`, and is reported. `Unknown` is not `Any` — it is what ty fills in where the
code said nothing, such as an unannotated `*args` parameter or a bare `tuple` annotation, and a
length is not worth complaining about when the contents were never stated either

```python
class Child(Base):
    def __init__(self, *args, **kwargs) -> None:
        super().__init__(*args, **kwargs)  # ok
```

an unannotated parameter is bounded by what its function's body asks of it under
[sound types](sound-types.md), and unpacking it is one of the things being asked — so it is not
reported either. a type variable someone wrote is, because its bound is a claim about every value
the parameter can take

```python
def f(key):
    row, col = key  # ok

def g[T: tuple[int, ...]](values: T):
    a, b = values  # error: `T@g` may not have exactly 2 elements
```

narrowing the length settles the question

```python
def f(values: tuple[int, ...]):
    if len(values) == 2:
        a, b = values  # ok — narrowed to `tuple[int, int]`
```

## turning it off

the check is on by default

```toml
[tool.ty.rules]
refutable-unpacking = "ignore"
```

a single site opts out with `# ty: ignore[refutable-unpacking]`
