# basedpython: keyword arguments in subscripts

`T[name=int]` binds the type parameter named `name` on `T` to `int`. for multi-typevar generics,
mixing positional and keyword arguments is permitted; for single-typevar generics, the keyword name
is dropped.

```toml
[environment]
python-version = "3.12"
```

## explicit binding for two-typevar class

```by
class M[K, V]: ...

def f(x: M[K=int, V=str]) -> None:
    reveal_type(x)  # revealed: M[int, str]
```

## single-typevar drops the keyword

```by
class B[T]: ...

def f(x: B[T=int]) -> None:
    reveal_type(x)  # revealed: B[int]
```

## a keyword argument is not a parameter field

a keyword argument shares its encoding with the labelled field of a parameter list and of an
anonymous named tuple. those two are always parenthesized, so an unparenthesized subscript slice
stays a keyword subscript.

```by
class M[K, V]: ...

def f(x: M[K=int, V=str], y: (int, /, name: str) -> bool, z: (a: int, b: str)) -> None:
    reveal_type(x)  # revealed: M[int, str]
    reveal_type(y)  # revealed: (int, /, name: str) -> bool
    reveal_type(z)  # revealed: (a: int, b: str)
```

## a keyword subscript of a value is checked as the `__getitem__` call it lowers to

```by
class A:
    def __getitem__(self, item: int) -> int:
        return 1

def f(a: A) -> None:
    # error: [missing-argument]
    # error: [unknown-argument]
    a[foo=1]
```

## a `__getitem__` that declares the keyword accepts it

```by
class A:
    def __getitem__(self, index: int, *, unit: str = "px") -> int:
        return index

def f(a: A) -> None:
    reveal_type(a[1, unit="em"])  # revealed: int
```

## the keyword's value is checked against its parameter

```by
class A:
    def __getitem__(self, *, unit: str) -> int:
        return 1

def f(a: A) -> None:
    a[unit=1]  # error: [invalid-argument-type]
```

## a keyword argument is checked against its type variable's bound

binding by name says nothing about what may be bound, so the bound still holds.

```by
class Bounded[T: int]: ...

def ok(x: Bounded[T=bool]) -> None: ...

# error: [invalid-type-arguments] "Type `str` is not assignable to upper bound `int` of type variable `T@Bounded`"
def bad(x: Bounded[T=str]) -> None: ...
```

## a keyword argument is checked against a bound range

the floor a [bound range](basedpython_bound_ranges.md) puts under the argument holds here too.

```by
class Base: ...
class Mid(Base): ...
class Leaf(Mid): ...

class Ranged[T: Mid..Base]: ...

def ok(x: Ranged[T=Mid]) -> None: ...

# error: [invalid-type-arguments] "Type `Leaf` does not satisfy lower bound `Mid` of type variable `T@Ranged`"
def bad(x: Ranged[T=Leaf]) -> None: ...
```

## a keyword argument is checked against a constraint list

```by
class Constrained[T in (int, str)]: ...

# error: [invalid-type-arguments] "Type `bytes` does not satisfy constraints `int`, `str` of type variable `T@Constrained`"
def f(x: Constrained[T=bytes]) -> None: ...
```

## a positional argument alongside a keyword one is checked too

one keyword argument routes the whole subscript through the by-name pipeline, so the arguments that
are still positional have to be checked there as well.

```by
class Pair[T: int, U: str]: ...

# error: [invalid-type-arguments] "Type `bytes` is not assignable to upper bound `int` of type variable `T@Pair`"
def f(x: Pair[bytes, U=str]) -> None: ...
```

## a type alias checks its bound by name as well

```by
type Alias[T: int] = list[T]

# error: [invalid-type-arguments] "Type `str` is not assignable to upper bound `int` of type variable `T@Alias`"
def f(x: Alias[T=str]) -> None: ...
```
