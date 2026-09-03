# basedpython: `T?` optional type

`T?` in a type position is the optional type `T | None`, surface syntax for `Optional[T]`.

```toml
[environment]
python-version = "3.12"
```

## bare optional annotation

```by
def f(x: int?) -> None:
    reveal_type(x)  # revealed: int | None
```

## optional in a return annotation

```by
def f() -> int?:
    return None

reveal_type(f())  # revealed: int | None
```

## optional parameter narrows like a union

```by
def f(x: int?) -> None:
    if x is not None:
        reveal_type(x)  # revealed: int
    else:
        reveal_type(x)  # revealed: None
```

## optional inside a generic

```by
def f(xs: list[int?]) -> None:
    reveal_type(xs[0])  # revealed: int | None
```

## optional of a union flattens

```by
def f(x: int | str?) -> None:
    reveal_type(x)  # revealed: int | str | None
```

## the marker after an arrow wraps the whole callable

an arrow's return type is read only as far as the first `|`, so a `?` written after one is over the
callable, not over what it returns. an optional return needs its own parentheses

```by
def f(g: (int) -> str?, h: (int) -> (str?)) -> None:
    reveal_type(g)  # revealed: ((int, /) -> str) | None
    reveal_type(h)  # revealed: (int, /) -> str | None
```

## the marker after a use-site type modifier is over the modified type

`literal str` is a single operand, so the `?` stands outside it

```by
def f(x: literal str?) -> None:
    reveal_type(x)  # revealed: LiteralString | None
```

## a `None` written mid-union stays where it was written

`?` takes the union to its left and the union then carries on, so the arms keep the order they were
written in

```by
def f(x: str? | int) -> None:
    reveal_type(x)  # revealed: str | None | int
```

## double optional is a distinct wrapped type

a single `T?` is the lossless union `T | None`, but a nested optional cannot collapse that way (the
outer- and inner-`None` states would merge). so `int??` is a distinct wrapped type, rendered in `?`
notation, and `int?? != int | None`

```by
def g() -> int??:
    return None

reveal_type(g())  # revealed: int??
```

each extra layer adds another `?`:

```by
def h() -> int???:
    return None

reveal_type(h())  # revealed: int???
```

## `?` over a bare type variable is the wrapped form

specializing a plain `T | None` with an optional `T` would flatten the layer, so `T?` denotes
`WrappedOptional(T | None)`: calling with `T = int | None` yields `int??` — the outer absence and
the present-inner-`None` stay distinguishable. the function constructs its result with `Some(…)` /
`None` (the wrapped runtime convention), and a bare `return t` is rejected:

```by
def f[T](t: T) -> T?:
    return Some(t)

def g(x: int?) -> None:
    reveal_type(f(x))  # revealed: int??

x: int?? = f(1)
```

returning the unwrapped value is an error — the wrapper is what preserves the layer:

```by
def bad[T](t: T) -> T?:
    return t  # error: [invalid-return-type]
```

## `Self?` is a plain union

`Self` stands for the enclosing class, which can never itself be an optional, so there is no inner
layer for the wrapper to keep apart: a fallible constructor returns its instance as it built it.

```by
class Record:
    def __init__(self, name: str) -> None:
        self.name = name

    class def parse(cls, line: str) -> Self?:
        if not line:
            return None
        return cls(line)

def f() -> None:
    record = Record.parse("a")
    reveal_type(record)  # revealed: Record | None
```

## wrapped optionals are covariant in their inner type

a narrower wrapped optional is assignable to a wider one (`Literal[1]` wrapped, to `int??` — see `x`
above), and a bare value is *not* assignable to a wrapped type (it carries no wrapper):

```by
# error: [invalid-assignment]
y: int?? = 5
```

## `?.` on a wrapped optional reaches the present value

the chain short-circuits on the wrapper's absent `None` and reads the attribute through the present
value (the runtime unwraps with `.value`):

```by
class A:
    v: int = 7

def f[T](t: T) -> T?:
    return Some(t)

def g(a: A):
    w = f(a)
    reveal_type(w?.v)  # revealed: int | None
```

## a chain over a wrapped optional runs to the end of its trailers

peeling the wrapper opens a chain like any other `?.`, so the trailers that follow are resolved
against the present value and only the end result is unioned with `None`.

```by
class Inner:
    code: str

class A:
    inner: Inner = Inner()

    def get(self) -> Inner:
        return self.inner

def f[T](t: T) -> T?:
    return Some(t)

def g(a: A):
    w = f(a)
    reveal_type(w?.inner.code)  # revealed: str | None
    reveal_type(w?.get())       # revealed: Inner | None
    reveal_type(w?.get().code)  # revealed: str | None
```

## force-unwrap `!` peels one optional layer

`expr!` removes one layer of optionality: a wrapped optional yields the next layer in, and a plain
`T | None` yields the present value `T`

```by
def g() -> int??:
    return Some(5)

result = g()
reveal_type(result)  # revealed: int??
reveal_type(result!)  # revealed: int | None
reveal_type(result!!)  # revealed: int
```

## propagate `^` peels one optional layer

`expr^` unwraps the present value (early-returning the absent value from the enclosing function), so
its type is the unwrapped value — the same peel as `!`

```by
def f() -> int?:
    return None

def g() -> int?:
    x = f()^
    reveal_type(x)  # revealed: int
    return x
```

## `^` / `!` on a result-like union peels the error arm

a result-like union (`T | E`, the error arm a `BaseException` subtype) is the unwrapped shape of a
`T ? E` result. `^` and `!` strip the exception arm, leaving the value type — the transpiler lowers
the guard to `isinstance(_, BaseException)` rather than `is None`

```by
def f() -> int | TypeError:
    return 1

def m() -> int | TypeError:
    x = f()^
    reveal_type(x)  # revealed: int
    return x

def n(r: str | ValueError) -> str | ValueError:
    reveal_type(r!)  # revealed: str
    return r
```

a union mixing both an error arm and `None` peels both:

```by
def p(r: int | None | TypeError) -> int | None | TypeError:
    reveal_type(r!)  # revealed: int
    return r
```

## a wrapped optional has no unwrapping methods

the operators are the whole surface: there is no `get`, `unwrap` or `value` accessor to reach the
present value with. an attribute lookup on a wrapped optional resolves against the wrapper's own
members and nothing else, so an invented accessor is an error rather than a silently gradual type

```by
def f() -> int??:
    return Some(1)

# error: [unresolved-attribute]
reveal_type(f().get())  # revealed: Unknown
# error: [unresolved-attribute]
reveal_type(f().value)  # revealed: Unknown
# error: [unresolved-attribute]
reveal_type(f().unwrap())  # revealed: Unknown
```

the members the wrapper does have still resolve:

```by
def f() -> int??:
    return Some(1)

reveal_type(f().__class__)  # revealed: <class 'object'>
reveal_type(f().__hash__())  # revealed: int
```

## `Some` is magically available

`Some` is the present-case optional constructor. It has no runtime definition in real Python — the
transpiler lowers `Some(x)` to the injected `Optional(x)` wrapper — so ty resolves it magically in
basedpython files (no import, not in any stub) rather than reporting an unresolved reference

```by
a = Some(None)
b = Some(1)
```

it takes exactly one value, so a missing or extra argument is an error:

```by
# error: [missing-argument]
a = Some()
# error: [too-many-positional-arguments]
b = Some(1, 2)
```

a local binding still shadows it:

```by
Some = 3
reveal_type(Some)  # revealed: 3
```

## passing an optional where `object` is expected is flagged

`object` absorbs the `None` arm silently, so passing an optional to an `object` parameter loses the
information that the value could be absent. the use is flagged where the optional is consumed as
`object`, and the user is nudged toward `!` (unwrap) or `cast object` (make it explicit).

```by
def sink(o: object): ...

def f(x: int?):
    # error: [optional-object-conversion] "Optional `int | None` is implicitly widened to `object`"
    sink(x)
```

## unwrapping or an explicit cast silences the warning

```by
def sink(o: object): ...

def f(x: int?):
    sink(x!)
    sink(x cast object)
```

## a non-optional value passed to `object` is never flagged

a plain value, or a bare `None`, has no optional layer to lose:

```by
def sink(o: object): ...

def f():
    sink(3)
    sink(None)
```

## assigning to a variable is not the trigger — the use is

the target of `a: object = x` narrows back to the optional type, so nothing is lost at the
assignment. the error instead surfaces at each use of the narrowed value as `object`.

```by
def sink(o: object): ...

def f(x: int?):
    a: object = x
    # error: [optional-object-conversion] "Optional `int | None` is implicitly widened to `object`"
    sink(a)
```

## a lesser depth of optional is flagged too

widening drops a *layer* of optionality: passing `object??` (rendered `object?`) to an `object`
parameter still discards the outer layer.

```by
def sink(o: object): ...

def f(x: object??):
    # error: [optional-object-conversion] "Optional `object?` is implicitly widened to `object`"
    sink(x)
```

## passing an optional to a parameter of equal depth stays silent

the receiving parameter preserves the `None` arm, so nothing is lost:

```by
def sink(o: int?): ...
def wide(o: int | str | None): ...

def f(x: int?):
    sink(x)
    wide(x)
```
