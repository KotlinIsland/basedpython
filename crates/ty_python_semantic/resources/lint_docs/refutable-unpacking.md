## What it does

Checks for an unpacking assignment whose value is not known to have the number of elements the
targets require.

## Why is this bad?

`a, b = value` binds both names unconditionally, but the unpacking only succeeds if `value` yields
exactly two elements. A `tuple[int, ...]`, a `list[int]`, or any other iterable whose length is not
part of its type satisfies the annotation at every length, so nothing rules out a `ValueError` at
runtime.

A starred target absorbs any number of elements, so it only requires the ones around it:
`a, *rest = value` still needs at least one element, and reports for the same reason. A splatted
argument is the same question against a parameter list: `f(*value)` binds the parameters
positionally, so a length that does not match raises `TypeError` rather than `ValueError`.

Three values are left alone: one whose type is `Any`, which has opted out of checking altogether;
one whose element type is `Unknown`, which ty fills in where the code said nothing at all; and an
unannotated parameter, whose type is bounded by what its function's body asks of it — including the
unpacking itself.

## Examples

```python
def f() -> tuple[int, ...]:
    return ()


def take(a: int, b: int) -> None: ...


a, b = f()  # error: [refutable-unpacking]
take(*f())  # error: [refutable-unpacking]
```

Give the value a length the type carries, or narrow it to one:

```python
def f() -> tuple[int, int]:
    return (1, 2)


a, b = f()


def g(values: tuple[int, ...]) -> None:
    if len(values) == 2:
        c, d = values  # ok — narrowed to `tuple[int, int]`
```
