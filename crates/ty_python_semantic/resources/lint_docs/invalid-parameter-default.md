## What it does

Checks for default values that can't be assigned to the parameter's annotated type.

A default only needs to fit *some* specialization of the type variables the annotation names: a call
that leaves the argument out is solved as if the default had been passed. The default is reported
when no such specialization exists. Only the type variables of the function itself (and, for a
method, of its class) can be chosen that way; one of an enclosing function is fixed for every call.
A default whose own type names one of those type variables, such as `[1]` inferred against `list[T]`
as `list[T | int]`, is reported too: it is a single value shared by every call, so no call can make
it fit.

In basedpython, a default an override inherits from the method it overrides is checked against the
override's annotation in the same way.

## Why is this bad?

This breaks the rules of the type system and weakens a type checker's ability to accurately reason
about your code.

## Examples

```python
from typing import TypeVar

T = TypeVar("T")
S = TypeVar("S", bound=str)


def f(a: int = ""): ...  # error


# fine: `g()` solves `T` from the default
def g(a: T = 1) -> T:
    return a


# no `S` bounded by `str` holds `1`
def h(a: S = 1) -> S:  # error
    return a
```
