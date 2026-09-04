# basedpython: type parameter bound ranges

a type parameter bound is written `T: Upper`, which constrains the top of the range only. a
basedpython bound range `T: Lower..Upper` also constrains the bottom, so `Lower` is assignable to
every specialization of `T`. both ends are required — `T: ..Upper` is spelled `T: Upper`.

```toml
[environment]
python-version = "3.12"
```

## the lower end admits values of that type inside the generic

a plain upper bound says nothing about what a `T` can hold, since `T` may still be specialized to
`Never`. a lower bound puts a floor under it.

```by
class WithRange[T: str..object]:
    def f(self) -> T:
        return "a"

class WithoutRange[T: object]:
    def f(self) -> T:
        # error: [invalid-return-type] "Return type does not match returned value"
        return "a"
```

## the lower end is a lower bound, not an equality

anything at or above `str` is a valid specialization.

```by
class C[T: str..object]: ...

def f(a: C[str], b: C[object], c: C[str | int], d: C[Any]): ...
```

## a specialization below the lower end is rejected

```by
class C[T: str..object]: ...

# error: [invalid-type-arguments] "Type `int` does not satisfy lower bound `str` of type variable `T@C`"
def f(c: C[int]): ...
```

## the upper end still applies

```by
class C[T: str..str]: ...

# error: [invalid-type-arguments] "Type `object` is not assignable to upper bound `str` of type variable `T@C`"
def f(c: C[object]): ...
```

## ranges work on functions too

```by
def f[T: str..object](x: T) -> T:
    return "a"

reveal_type(f("a"))  # revealed: str
```

## an empty range is an error

no type is both above `object` and below `str`, so the type variable could never be specialized.

```by
# error: [invalid-type-variable-bound] "TypeVar lower bound `object` is not assignable to its upper bound `str`"
class C[T: object..str]: ...
```

## the lower end must accept the default

a default is a specialization like any other, so it has to sit inside the range. without this the
declared default could name a type the body was never checked against.

```by
# error: [invalid-type-variable-default] "TypeVar default is not assignable from the TypeVar's lower bound"
class C[T: str..object = int]:
    def f(self) -> T:
        return "a"
```

## the upper end must still accept the default

```by
# error: [invalid-type-variable-default] "TypeVar default is not assignable to the TypeVar's upper bound"
class C[T: str..str = object]: ...
```

## `Self` is a valid lower end

`Self` is bound by the enclosing class rather than by the generic context being declared, so it is
exempt from the generic-bound rule at both ends.

```by
class C:
    def f[T: Self..object](self) -> T:
        return self
```

## the upper end still governs member access

a range says nothing new about what a `T` *has* — that is the upper end's job, exactly as for a
plain bound.

```by
class C[T: str..object]:
    def f(self, x: T) -> int:
        # error: [invalid-argument-type] "Argument to function `len` is incorrect"
        return len(x)

class D[T: str..Sized]:
    def f(self, x: T) -> int:
        return len(x)
```

## narrowing works through a range

```by
class C[T: str..object]:
    def f(self, x: T):
        if isinstance(x, int):
            reveal_type(x)  # revealed: T@C & int
        else:
            reveal_type(x)  # revealed: T@C & not int
```

## variance is unaffected

both ends are ordinary bounds, so they do not constrain which variances are declarable.

```by
class Co[out T: str..object]: ...
class Contra[in T: str..object]: ...
class Inv[in out T: str..object]: ...

def f(a: Co[str], b: Contra[object], c: Inv[str]):
    d: Co[object] = a
```

## an inherited generic does not carry the range

`D`'s own `U` is unbounded, so it does not inherit `C`'s floor; the base specialization is what gets
checked.

```by
class C[T: str..object]: ...

# error: [invalid-type-arguments] "Type `U@D` does not satisfy lower bound `str` of type variable `T@C`"
class D[U](C[U]): ...

class E[U: str..object](C[U]): ...
```

## a bound range needs a plain upper end

a parameter list is not a type, so it cannot cap a range of types.

```by
# error: [invalid-type-variable-bound] "TypeVar bound range requires a plain upper bound as its upper end"
class C[T: int..(*: *, **: *)]: ...
```

## a generic lower bound is rejected

```by
class C[T]:
    # error: [invalid-type-variable-bound] "TypeVar lower bound cannot be generic"
    def f[U: T..object](self, x: U) -> U:
        return x
```

## both ends are required

```by
# error: [invalid-syntax] "Type parameter bound range requires both a lower and an upper bound, as in `T: int..object`"
class C[T: str..]: ...
```

```by
# error: [invalid-syntax] "Type parameter bound range requires both a lower and an upper bound, as in `T: int..object`"
class D[T: ..object]: ...
```

## ranges are basedpython-only

```py
# error: [invalid-syntax] "type parameter bound ranges are not valid in `.py` files"
class C[T: str..object]: ...
```

## a range composes with a default

```by
class C[T: str..object = str]: ...

def f(c: C):
    reveal_type(c)  # revealed: C[str]
```

## a range accepts a default that names the type variable

A later type parameter's default can be written in terms of an earlier one, as `B = Box[T]` is here.
At the point that default is checked, `T` has no binding context yet, so the check binds a copy of
the default before measuring it against the range. Without that, the bare type variable rather than
the type it stands for reaches the lower end, and the specialization is rejected.

```by
class Box[T: str..object]: ...

class Holder[T: str..object, B = Box[T]]: ...

reveal_type(Holder[str]())  # revealed: final Holder[str, Box[str]]
```

## `..` outside a bound says so

Anywhere but a type parameter's bound, `Lower..Upper` is not a range. Left alone it parses as two
attribute accesses with an empty name between them, which earns a pair of diagnostics about an
attribute nobody wrote.

```by
# error: [invalid-syntax] "a `..` bound range is only valid in a type parameter's bound"
# error: [invalid-syntax] "Expected a statement"
x: str..object
```
