# basedpython: bounds on a variadic pack

a variadic pack may carry an upper bound, and the star count on the bound decides what it
constrains. an unstarred bound bounds every *member* of the pack — every element of a `*Ts: int`,
every field of a `**Kwargs: int`. a starred one bounds the pack *as a whole*, taking the same star
count the pack was declared with: `*Ts: *(int, str)` and `**Kwargs: **{"a": int}`

CPython rejects a bound on either kind of pack, so both forms are basedpython-only

```toml
[environment]
python-version = "3.12"
```

## an unstarred `TypeVarTuple` bound applies to every element

```by
class A[*Ts: int]: ...

def ok(a: A[int, bool]): ...

# error: [invalid-type-arguments] "Type `str` is not assignable to upper bound `int` of type variable tuple `Ts@A`"
def bad(a: A[int, str]): ...
```

## a starred `TypeVarTuple` bound applies to the whole pack

```by
class A[*Ts: *(int, str)]: ...

def ok(a: A[int, str]): ...

# the pack is a tuple, so the bound is covariant in its elements
def narrower(a: A[bool, str]): ...

# error: [invalid-type-arguments] "Type `(int, bytes)` is not assignable to upper bound `(int, str)` of type variable tuple `Ts@A`"
def wrong_element(a: A[int, bytes]): ...
```

## a whole-pack bound constrains the pack's length

```by
class A[*Ts: *(int, str)]: ...

# error: [invalid-type-arguments] "Type `(int,)` is not assignable to upper bound `(int, str)` of type variable tuple `Ts@A`"
def too_short(a: A[int]): ...
```

## a variable-length whole-pack bound admits any length

```by
class A[*Ts: *tuple[int, ...]]: ...

def one(a: A[int]): ...
def many(a: A[int, bool, int]): ...

# error: [invalid-type-arguments] "Type `(int, str)` is not assignable to upper bound `(*: int)` of type variable tuple `Ts@A`"
def wrong_element(a: A[int, str]): ...
```

## a starred bound must still unpack something

```by
# error: [invalid-type-form] "`*` can only unpack a tuple type or `TypeVarTuple`"
class A[*Ts: *int]: ...
```

## an unstarred keyword-pack bound applies to every field

```by
class A[**Kwargs: int]: ...

def ok(a: A[x=int, y=bool]): ...

# error: [invalid-type-arguments] "Type `str` is not assignable to upper bound `int` of keyword-variadic pack `Kwargs@A`"
def bad(a: A[x=int, y=str]): ...
```

## a starred keyword-pack bound names the fields the pack must have

```by
class A[**Kwargs: **{"a": int}]: ...

def ok(a: A[a=int]): ...

# an upper bound is a *lower* limit on the shape, so extra fields are fine
def extra(a: A[a=int, b=str]): ...

# error: [invalid-type-arguments] "Type `str` is not assignable to upper bound `{"a": int}` of keyword-variadic pack `Kwargs@A`"
def wrong_field_type(a: A[a=str]): ...
```

## a missing field is reported by name

```by
class A[**Kwargs: **{"a": int}]: ...

# error: [invalid-type-arguments] "Upper bound `{"a": int}` of keyword-variadic pack `Kwargs@A` requires a field `a`"
def missing(a: A[b=str]): ...
```

## the empty pack has no fields at all

```by
class A[**Kwargs: **{"a": int}]: ...

# error: [invalid-type-arguments] "Upper bound `{"a": int}` of keyword-variadic pack `Kwargs@A` requires a field `a`"
def empty(a: A[()]): ...
```

## a named `TypedDict` works as a whole-pack bound

```by
from typing import TypedDict

class TD(TypedDict):
    a: int

class A[**Kwargs: **TD]: ...

def ok(a: A[a=int]): ...

# error: [invalid-type-arguments] "Type `str` is not assignable to upper bound `TD` of keyword-variadic pack `Kwargs@A`"
def bad(a: A[a=str]): ...
```

## a whole-pack keyword bound has to have fields

```by
# error: [invalid-type-variable-bound] "The whole-pack bound of a keyword-variadic pack must be a dict literal type or a `TypedDict`, not `int`"
class A[**Kwargs: **int]: ...
```

## an inferred pack is checked against the bound too

a pack solved from a call's keyword arguments is checked where it is solved, not only where it is
written out.

```by
class A[**Kwargs: int]:
    init(**kwargs: **Kwargs)

ok = A(x=1, y=True)
reveal_type(ok)  # revealed: final A[x=int, y=bool]

# error: [invalid-argument-type] "Type `str` is not assignable to upper bound `int` of keyword-variadic pack `Kwargs@A`"
bad = A(x=1, y="s")
```

## an inferred pack is checked against a whole-pack bound

```by
class A[**Kwargs: **{"a": int}]:
    init(**kwargs: **Kwargs)

ok = A(a=1)
reveal_type(ok)  # revealed: final A[a=int]

# error: [invalid-argument-type] "Upper bound `{"a": int}` of keyword-variadic pack `Kwargs@A` requires a field `a`"
missing = A(b=1)
```

## a bounded pack is still a pack, not a `TypedDict` unpack

a `**kwargs: Unpack[T]` with `T` bounded by a `TypedDict` is python's PEP-692 shape; a pack bounded
by a dict literal type is not, and is still solved as a parameter list.

```by
class A[**Kwargs: **{"a": int}]:
    def get(self) -> (**Kwargs) -> None:
        raise NotImplementedError

def f(a: A[a=int, b=str]):
    reveal_type(a.get())  # revealed: (*, a: int, b: str) -> None
```

## a `TypeVarTuple` bound is not valid in a `.py` file

```py
# error: [invalid-syntax] "a bound on a `TypeVarTuple` is a basedpython feature and is not valid in .py files"
type X[*Ts: int] = int
```

## a starred `TypeVarTuple` bound is not valid in a `.py` file either

the bound is still parsed, so its contents get python's own reading — a parenthesized tuple is not a
type expression there.

```py
# error: [invalid-syntax] "a bound on a `TypeVarTuple` is a basedpython feature and is not valid in .py files"
# error: [invalid-type-form] "Tuple literals are not allowed in this context in a type expression"
type X[*Ts: *(int, str)] = int
```

## a keyword-pack bound is not valid in a `.py` file

```py
# error: [invalid-syntax] "a bound on a keyword-variadic pack is a basedpython feature and is not valid in .py files"
type X[**P: int] = int
```

## a starred keyword-pack bound is not valid in a `.py` file either

```py
# error: [invalid-syntax] "a bound on a keyword-variadic pack is a basedpython feature and is not valid in .py files"
# error: [invalid-type-form] "Dict literals are not allowed in type expressions"
# error: [invalid-type-variable-bound]
type X[**P: **{"a": int}] = int
```
