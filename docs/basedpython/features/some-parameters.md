# `some` parameters

`some T` in a parameter's annotation gives that parameter a type parameter of its own, bounded by
`T` and named after the parameter. the rest of the signature names that type by the parameter's
name:

```by
def echo(s: some str) -> s:
    return s

echo("hi")  # "hi"
```

transpiles to:

```python
from typing import TypeVar
_s = TypeVar("_s", bound=str)
def echo(s: _s) -> _s:
    return s
```

`echo` accepts any `str` and returns the type it was called with — a literal stays a literal, a
subclass stays the subclass. it is `def echo[S: str](s: S) -> S` without the type parameter list

## what it means

- each `some` parameter is a type parameter of its own, solved from its argument at the call:

    ```by
    def pair(a: some int, b: some str) -> tuple[a, b]:
        return (a, b)

    pair(1, "x")  # (1, "x")
    ```

- an argument outside the bound is an `invalid-argument-type` error, as it is for any bounded type
    parameter

- the bound is any type expression, a union or an optional among them: `n: some int?`

- in the body, the parameter's name is the parameter. `return s` returns the argument; only the
    signature reads `s` as a type

- `some` is written at the top of a parameter's annotation. it is not a type of its own, and cannot
    be nested inside one (`list[some int]`), written as a return type or on a variable. anywhere else
    it is a syntax error: "`some` is only allowed at the top of a parameter annotation"

- `some` is not written on `*args` or `**kwargs`. their annotation is the type of each element, while
    the parameter is a tuple or a dict of them, so the parameter's name could not name the type. a
    type parameter in a list says which is meant: `def first[T: int](*xs: T) -> T`

## on older pythons

the type parameter is always declared as a `TypeVar`, whatever the target — native type parameter
syntax has nowhere to put one the source never wrote in a list. a bound python cannot evaluate on
the target is spelled the way it can: `some int?` is `bound=Union[int, None]` below 3.10

## in inferred signatures

an unannotated parameter's inferred type is shown in the same notation, since it is the same thing: a
type parameter bounded by what the body needs. see [sound types](sound-types.md)

```by
def twice(n = 1):
    return n * 2

reveal_type(twice)  # def twice(n: some int = 1) -> n * 2
```

## see also

- [generics](generics.md) — declaring type parameters in a list
