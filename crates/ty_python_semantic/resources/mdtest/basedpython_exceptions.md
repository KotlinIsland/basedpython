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

The `try` body has to be able to raise, or its handler is never entered and nothing after it is
analysed either.

```by
def risky() -> int:
    raise ValueError

def f():
    raise TypeError

def main():
    try:
        # error: [unhandled-exception] "`ValueError` can escape `main`, the entry point"
        risky()
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

## a generic function's clause is inferred once

A generic function's signature is inferred in its type-parameter scope, while a non-generic one's is
deferred to the enclosing scope. Both reach the same inference, so the clause must not also be
inferred on the way there. `raises ...` is the case that shows it: the ellipsis is the gradual
exception set rather than a type expression, so it is inferred as the plain value it is, and
inferring one expression twice in a single region is a hard error.

```by
def gradual[T](value: T) -> T raises ...:
    raise TypeError
```

The clause still reaches the check on the body, which is what says it was inferred at all.

```by
def declared[T](value: T) -> T raises TypeError:
    # error: [undeclared-raise] "`declared` can raise `ValueError`, which its `raises` clause does not include"
    raise ValueError
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

## a type parameter in a clause must be bounded by an exception

A type parameter stands for one type the caller chooses, so `raises T` says something about
exceptions only when every type `T` can be is an exception. That is what its declaration says: a
parameter with no bound at all can be `int` as easily as `OSError`.

```by
# error: [invalid-raises-clause] "`T@f` is not always an exception, so it cannot appear in a `raises` clause"
def f[T](value: T) raises T:
    return
```

A set of constraints has to be all exceptions, since the caller can pick any one of them.

```by
# error: [invalid-raises-clause] "`T@g` is not always an exception, so it cannot appear in a `raises` clause"
def g[T in (int, KeyError)](value: T) raises T:
    return
```

An upper bound of `BaseException`, or of any exception below it, is enough, and so is a set of
constraints that are all exceptions.

```by
def bounded[T: OSError](value: T) raises T:
    return

def constrained[T in (KeyError, IndexError)](value: T) raises T:
    return
```

Only a member of the set itself has to be an exception. A type parameter inside one is a type
argument of an exception class, and `E[int]` is as much an exception as `E[OSError]`.

```by
class E[X](Exception): ...

def wrapped[X](value: X) raises E[X]:
    return
```

## a call reads a clause's type parameter as what it solved

The callee writes its exception set in terms of its own type parameters, so what escapes a
particular call is that set with what the call solved them to. `raise_it(TypeError())` raises a
`TypeError`, and nothing else.

```by
def raise_it[T: BaseException](error: T) raises T:
    raise error

def caller() raises TypeError:
    raise_it(TypeError())

def wrong() raises ValueError:
    # error: [undeclared-raise] "`wrong` can raise `TypeError`, which its `raises` clause does not include"
    raise_it(TypeError())
```

A body with no clause of its own is read the same way, since the set recovered from it is in the
same type parameters.

```by
def undeclared[T: BaseException](error: T):
    raise error

def calls_undeclared() raises KeyError:
    undeclared(KeyError())
```

The caller's own type parameters survive: a call that solves the callee's `T` to the caller's `U`
raises `U`, which is exactly what the caller declared.

```by
def forwards[U: BaseException](error: U) raises U:
    raise_it(error)
```

## a recursive call raises what it solves the parameter to

A call back into the same function is read the same way. When it solves the type parameter to
something else, it raises that something else: `f(KeyError(), False)` raises a `KeyError` whatever
the outer call was made with.

```by
def f[T: BaseException](error: T, again: bool) raises T:
    if again:
        # error: [undeclared-raise] "`f` can raise `KeyError`, which its `raises` clause does not include"
        f(KeyError(), False)
    raise error
```

A body with no clause gets the same answer, and passes it on to its callers.

```by
def g[T: BaseException](error: T, again: bool):
    if again:
        g(KeyError(), False)
    raise error

def caller() raises ValueError:
    # error: [undeclared-raise] "`caller` can raise `KeyError`, which its `raises` clause does not include"
    g(ValueError(), True)
```

## a closure names its enclosing function's type parameter

A function nested in a generic one can raise a value of the enclosing function's type parameter.
Where the closure is called, that parameter is still in scope and still means what the enclosing
function was called with, so it stays as it is.

```by
def outer[T: BaseException](error: T) raises T:
    def inner():
        raise error
    inner()
```

## a class's type parameter in a clause is read through the receiver

A method may name its class's type parameter, and the receiver is what says which exception that is.
It is found wherever in the receiver's ancestry the method was declared, and a call through the
class itself reads it the same way.

```by
class Reader[T: BaseException]:
    def read(self) raises T:
        return

class FileReader(Reader[OSError]):
    pass

def read_one(reader: Reader[KeyError]) raises KeyError:
    reader.read()

def read_file(reader: FileReader) raises OSError:
    reader.read()

def read_unbound(reader: Reader[KeyError]) raises KeyError:
    Reader[KeyError].read(reader)
```

A classmethod has no instance at all, and the class it is called through says which exception it is.

```by
class Source[T: BaseException]:
    @classmethod
    def open(cls) raises T:
        return

def open_one() raises KeyError:
    Source[KeyError].open()
```

## an explicit specialization is read like a solved call

Specializing a function explicitly names its type parameter where a call would otherwise solve it,
and the call raises what was named.

```by
def raise_os[T: OSError](error: T) raises T:
    raise error

def explicit() raises FileNotFoundError:
    raise_os[FileNotFoundError](FileNotFoundError())

def wrong() raises PermissionError:
    # error: [undeclared-raise] "`wrong` can raise `FileNotFoundError`, which its `raises` clause does not include"
    raise_os[FileNotFoundError](FileNotFoundError())
```

The specialization belongs to the function's type, so it is still there when that is held in a
variable, and a caller can specialize with its own type parameter.

```by
def aliased() raises FileNotFoundError:
    raise_found = raise_os[FileNotFoundError]
    raise_found(FileNotFoundError())

def forwards[U: OSError](error: U) raises U:
    raise_os[U](error)
```

## a call that does not bind raises nothing known

A call that does not type-check solves nothing, so its callee's type parameter says nothing about
what it raises. The call is already reported, and the exception analysis adds nothing to it.

```by
def raise_it[T: BaseException](error: T) raises T:
    raise error

def caller() raises Never:
    raise_it(1)  # error: [invalid-argument-type]
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
