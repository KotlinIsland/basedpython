# basedpython: exception tracking

Every function has an exception set: the exceptions that can escape a call to it. It is inferred
from the body — `raise`, `assert`, and calls to functions whose bodies are visible — and narrowed by
`try` / `except`. A `raises` clause declares the set instead, and the body is checked against it.

The clause holds an ordinary type expression, so `Never` cannot raise, `A | B` is a union,
`not TypeError` is everything but that, and `...` opts out.

```toml
[environment]
python-version = "3.12"
```

## exceptions propagate through calls

An undeclared function simply propagates what it can raise to its callers, so neither `f` nor `g` is
an error. `main` is the entry point and has no caller to propagate to.

```by
def f():
    raise TypeError

def g():
    f()

def h():
    try:
        g()
    except TypeError:
        pass

def main():
    # error: [unhandled-exception] "`TypeError` can escape `main`, the entry point"
    g()
```

## a handled exception does not escape

```by
def raises_type_error():
    raise TypeError

def main():
    try:
        raises_type_error()
    except TypeError:
        pass
```

## only the handled part of a union stops escaping

```by
def both():
    if True:
        raise TypeError
    raise ValueError

def main():
    try:
        # error: [unhandled-exception] "`ValueError` can escape `main`, the entry point"
        both()
    except TypeError:
        pass
```

## a broader handler catches a subclass

```by
def f():
    raise TypeError

def main():
    try:
        f()
    except Exception:
        pass
```

## a bare except catches everything

```by
def f():
    raise SystemExit

def main():
    try:
        f()
    except:
        pass
```

## a handler body raises on its own

```by
def f():
    raise TypeError

def main():
    try:
        f()
    except TypeError:
        # error: [unhandled-exception] "`ValueError` can escape `main`, the entry point"
        raise ValueError
```

## else and finally are not protected by the handlers

```by
def f():
    raise TypeError

def main():
    try:
        pass
    except TypeError:
        pass
    else:
        # error: [unhandled-exception] "`TypeError` can escape `main`, the entry point"
        f()
    finally:
        # error: [unhandled-exception] "`TypeError` can escape `main`, the entry point"
        f()
```

## a declared clause is checked against the body

```by
def f() raises TypeError:
    raise TypeError

def g() raises TypeError:
    # error: [undeclared-raise] "`g` can raise `ValueError`, which its `raises` clause does not include"
    raise ValueError

def h() -> int raises TypeError | ValueError:
    if True:
        raise TypeError
    raise ValueError
```

## a declaration is what callers see

`declared` can only raise `TypeError` as far as its callers are concerned, whatever its body does.

```by
def declared() raises TypeError:
    raise TypeError

def main():
    try:
        declared()
    except TypeError:
        pass
```

## `raises Never` cannot raise

```by
def pure() raises Never:
    return

def impure() raises Never:
    # error: [undeclared-raise] "`impure` can raise `TypeError`, which its `raises` clause does not include"
    raise TypeError
```

## `raises ...` opts out of tracking

```by
def anything(fail: bool) raises ...:
    if fail:
        raise TypeError

def main():
    anything(False)
```

## a negated clause is checked, but strictly

`not TypeError` is the ordinary negation type, and the body is checked against it with ordinary
assignability. That is strict here: any two exception classes can be combined by a third that
inherits both, so `ValueError` is not *provably* outside `TypeError` and is reported too. Declaring
what a function does raise, or `raises Never`, is the practical way to rule an exception out.

```by
def quiet() raises not TypeError:
    return

def f() raises not TypeError:
    # error: [undeclared-raise] "`f` can raise `TypeError`, which its `raises` clause does not include"
    raise TypeError

def g() raises not TypeError:
    # error: [undeclared-raise] "`g` can raise `ValueError`, which its `raises` clause does not include"
    raise ValueError
```

## a clause with no exception in it is rejected

```by
# error: [invalid-raises-clause] "`int` contains no exception, so nothing can satisfy this `raises` clause"
def f() raises int:
    return
```

## `assert` raises `AssertionError`

```by
def check(value: int):
    assert value > 0

def main():
    # error: [unhandled-exception] "`AssertionError` can escape `main`, the entry point"
    check(1)
```

## a bare `raise` re-raises what the handler caught

```by
def f():
    raise TypeError

def main():
    try:
        f()
    except TypeError:
        # error: [unhandled-exception] "`TypeError` can escape `main`, the entry point"
        raise
```

## a nested function does not raise where it is defined

```by
def main():
    def inner():
        raise TypeError
```

## recursion terminates

a function that calls itself is followed once. each of these gets its own `main`, because a body
that always raises returns `Never` and so anything after the call would be unreachable

```by
def down(n: int):
    if n > 0:
        down(n - 1)
    raise ValueError

def main():
    # error: [unhandled-exception] "`ValueError` can escape `main`, the entry point"
    down(3)
```

## mutual recursion terminates

```by
def ping(n: int):
    pong(n)

def pong(n: int):
    if n > 0:
        ping(n - 1)
    raise TypeError

def main():
    # error: [unhandled-exception] "`TypeError` can escape `main`, the entry point"
    ping(3)
```

## a stub may declare what it raises

The default is what makes the feature usable, not a limit: a stub that declares a clause propagates
it to callers across modules, which is how a dependency opts its api in.

`reader.byi`:

```byi
def read() raises OSError: ...
def quiet() raises Never: ...
```

```by
from reader import read, quiet

def main():
    quiet()
    # error: [unhandled-exception] "`OSError` can escape `main`, the entry point"
    read()
```

## a tuple handler catches each of its members

```by
def both():
    if True:
        raise TypeError
    raise ValueError

def main():
    try:
        both()
    except (TypeError, ValueError):
        pass
```

## a tuple handler covering part of the union leaves the rest

```by
def both():
    if True:
        raise TypeError
    raise ValueError

def partial() raises Never:
    try:
        # error: [undeclared-raise] "`partial` can raise `ValueError`, which its `raises` clause does not include"
        both()
    except (TypeError,):
        pass
```

## a `with` body is tracked, its context manager is not

Entering and exiting a context manager can raise, and that is not yet modelled — but the body is
walked like any other.

```by
class CM:
    def __enter__(self) -> int:
        raise OSError

    def __exit__(self, *args: object) -> None: ...

def main():
    with CM() as value:
        # error: [unhandled-exception] "`TypeError` can escape `main`, the entry point"
        raise TypeError
```

## an overloaded function contributes every overload

Which overload a call matched is not known to this analysis, so the set is the union over all of
them — an upper bound, since naming an exception that cannot happen is safer than missing one that
can.

```by
def f(x: int) -> int raises TypeError
def f(x: str) -> str raises ValueError
def f(x: dynamic) -> dynamic raises TypeError | ValueError:
    raise TypeError

def main():
    # error: [unhandled-exception] "`TypeError | ValueError` can escape `main`, the entry point"
    f("s")
```

## mutual recursion between undeclared functions still reports

The set is a least fixed point, so an exception raised anywhere in a recursive group reaches every
caller of it.

```by
def a(n: int):
    if n > 0:
        b(n - 1)

def b(n: int):
    if n > 0:
        a(n - 1)
    raise ValueError

def caller() raises TypeError:
    # error: [undeclared-raise] "`caller` can raise `ValueError`, which its `raises` clause does not include"
    a(1)
```

## calls into stubs raise nothing

A function with no visible body — anything from a stub — contributes nothing, so the standard
library does not make every set `BaseException`.

```by
def main():
    print("hello")
    len([1, 2, 3])
```

## a method's raises are tracked

```by
class C:
    def m(self):
        raise TypeError

def main():
    c = C()
    # error: [unhandled-exception] "`TypeError` can escape `main`, the entry point"
    c.m()
```

## `main` may declare what it raises

Declaring the clause is opting in: `main` then reports against its own declaration rather than
against the entry-point rule.

```by
def fails(flag: bool):
    if flag:
        raise TypeError

def main() raises TypeError:
    fails(False)
```

## a `raises` clause is a .py syntax error

```py
# error: [invalid-syntax] "`raises` clauses are not valid in .py files"
def f() raises TypeError:
    raise TypeError
```
