# basedpython: literal-friendly annotations

basedpython diverges from PEP 484 stringified-forward-reference and PEP 586 literal rules:

- a string in annotation/type position is `Literal[<str>]`, not a forward reference. forward refs
    are unnecessary because basedpython annotations are always deferred
- float and complex literals are accepted in type position. python's `Literal[...]` has no spelling
    for one, so what they lower to is the `lowering.float-literals` option's to say: the nominal
    `float` / `complex` instance by default
- `A[T=int]` is a keyword type-arg binding, equivalent to `A[int]` for single-typevar generics

```toml
[environment]
python-version = "3.12"
```

## string in annotation is a Literal

```by
a: "asdf" = "asdf"
reveal_type(a)  # revealed: "asdf"
```

## string in subscript type position is a Literal

```by
from typing import Literal

x: Literal["a", "b"] = "a"
reveal_type(x)  # revealed: "a"
```

## a union of string literals is a union of literal types

`"foo" | "bar"` is two literal types joined, not a `str.__or__` that would fail at runtime: the
transpiler lowers the whole type expression to `Literal["foo", "bar"]` before python ever sees it.
that holds in an annotation and on the right-hand side of a type alias, whose value is the same
lowered expression.

```by
type Name = "foo" | "bar"

def f(a: Name, b: "spam" | "eggs") -> None:
    reveal_type(a)  # revealed: "foo" | "bar"
    reveal_type(b)  # revealed: "spam" | "eggs"

# error: [invalid-assignment]
c: Name = "baz"
```

## a float literal in a union is a union arm like any other

left bare, `int | 3.5` would be a `TypeError` the moment python evaluated the annotation. the
lowering is what keeps it running, and the checker reads the arm as the literal type it was written
as either way.

```by
def f(a: int | 3.5, b: int | 2j) -> None:
    reveal_type(a)  # revealed: int | 3.5
    reveal_type(b)  # revealed: int | 2j
```

## float literal in annotation is the literal type

```by
a: 1.5 = 1.5
reveal_type(a)  # revealed: 1.5
```

## complex literal in annotation is the literal type

```by
a: 2j = 2j
reveal_type(a)  # revealed: 2j
```

## float and complex value literals preserve type

```by
a = 1.1
reveal_type(a)  # revealed: 1.1

b = 2j
reveal_type(b)  # revealed: 2j
```

## keyword type-arg binding

```by
class A[T]: ...

def f(a: A[T=int]):
    reveal_type(a)  # revealed: A[int]
```

## keyword type-arg binding reorders by name

```by
class B[T, R]: ...

def f(a: B[R=str, T=int]):
    reveal_type(a)  # revealed: B[int, str]
```

## keyword type-arg binding falls back to typevar default

```toml
[environment]
python-version = "3.13"
```

```by
class C[T = int, R = str]: ...

def f(a: C[R=int]):
    reveal_type(a)  # revealed: C[int, int]
```

## forward self-reference works without quotes

basedpython annotations are deferred, so a class can refer to itself in its own method signatures
without stringification

```by
class A:
    def make(self) -> A:
        return A()

reveal_type(A().make())  # revealed: A
```
