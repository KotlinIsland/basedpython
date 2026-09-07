# type tests and identity

basedpython swaps the surface syntax for identity comparison and type tests:
`===` is identity, and `is` asks whether a value has a type

```by
if x === y:
    ...
if x !== y:
    ...
if x is int:
    ...
if x is not str:
    ...
```

transpiles to:

```python
if x is y:
    ...
if x is not y:
    ...
if isinstance(x, int):
    ...
if not isinstance(x, str):
    ...
```

| basedpython  | Python output          |
| ------------ | ---------------------- |
| `x === y`    | `x is y`               |
| `x !== y`    | `x is not y`           |
| `x is T`     | `isinstance(x, T)`     |
| `x is not T` | `not isinstance(x, T)` |

`T` is a *type*, and the check that comes out is whatever decides membership of
it — `isinstance` for a class, but an equality for a literal and an identity for
`None`. the sections below say which

## why

`isinstance(x, T)` is the dominant runtime check; `is` for object identity is
rare outside of `is None`. basedpython promotes the common case to a keyword
and demotes identity to a triple-equals operator borrowed from JavaScript

## the right-hand side is a type

`is` takes a *type expression*, the same thing an annotation takes. anything an
annotation can name, a test can test against:

```by
from typing import Literal

def f(v: object):
    if v is int | str: ...        # a union
    if v is list: ...             # a class, arguments and all
    if v is Literal[1, 2]: ...    # a set of values
    if v is type[int]: ...        # a class object
    if v is f"item-{int}": ...    # a string pattern
```

and anything an annotation rejects, a test rejects with the same message —
`v is os` names a module, which is not a type in either place

this is why `is` does not mean `isinstance`'s tuple: `isinstance(v, (int, str))`
spells "any of these", while `(int, str)` in a type expression is the *tuple
type*. write `v is int | str` for the first

## checking against `None` and other values

`None`, `True`, a number, a string and an enum member are all type expressions —
each names the type holding exactly that one value. so the check that comes out
is the identity or equality that decides membership of that type:

```by
if a is None: ...           # → a is None
if flag is True: ...        # → type(flag) is bool and flag == True
if g is Genre.A: ...        # → g is Genre.A
```

the class guard on a literal is not redundant: python's `1 == True` would
otherwise let a `bool` satisfy `Literal[1]`

## a target with no runtime form

a test narrows, so it has to *earn* its `True`. a target the runtime could only
partly check is rejected rather than approximated, by `erased-type-check` — and
the emitted python answers `False`, since a test that cannot be made is one
nothing satisfies:

```by
if v is Any: ...                  # error: admits every value
if v is Callable[[], int]: ...    # error: the signature is not recorded on the value
if v is Movie: ...                # error: a TypedDict's instances are plain dicts
```

a protocol is checkable when basedpython can see enough to check it: a
`@runtime_checkable` one gets the presence check python itself performs, and any
other is checked member by member against the value's
[reified annotations](reified-generics.md). one with a member the emitted python
cannot name is rejected

## a test that can never hold

`non-overlapping-type-test` warns when the value's type and the tested type share
no value at all. the test is then a constant — `is` never holds, `is not` always
does — so either the branch it guards is dead or the wrong type was named:

```by
class Shape: ...

def f(x: None, s: Shape):
    if x is int: ...      # warning: `None` and `int` are non-overlapping
    if s is str: ...      # warning: `Shape` and `str` are non-overlapping

def g(o: object):
    if o is int: ...      # ok — `object` overlaps `int`
```

the value's *narrowed* type is what is tested, so it is often sharper than the
declaration: `c = 1` is a `Literal[1]`, and a constructor call is
[`final A`](type-modifiers.md#a-constructor-call-is-inferred-final) — a value
whose runtime class is exactly `A`'s, and therefore not a `str` and not a
subclass of `A` either.

a [parametric target](parametric-type-tests.md) is judged by the same fold that
decides the test, so a use-site variance projection (`a is A[out int]`) that
makes the test possible keeps it quiet. a union target is reported only when no
arm can hold: any arm matching makes the whole test hold

## a settled test is its answer

where the value's type decides the question, the test *is* that answer, and the
branch it guards is decided with it:

```by
def f(x: int):
    reveal_type(x is int)  # revealed: True
```

an undecidable test is `bool`. the identity folds python applies to the same
operator have no place here: the right-hand side names a type rather than the
class object the same source spells as a value

## chaining

a type test may not join a chained comparison. python chains `a is int is str`
into `a is int and int is str`, whose second half asks whether the *class* `int`
has the type `str` — never what the writer meant, so it is a syntax error. split
it into separate tests joined with `and`. a chain of `===` / `!==` is ordinary
python and stays legal

## interaction with `==`

`==` is unchanged — it still calls `__eq__` exactly as in Python. only `is`
and `===` are remapped

## scope

the swap applies to every comparison in source. there is no opt-out at the
statement level — write `===` / `!==` whenever you mean identity. ty understands
both forms when type-checking `.by` files
