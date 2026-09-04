# basedpython: reified class type parameters

A PEP 695 class type parameter is *reified* when it is declared `reified`, or when the class reads
it in a value position — anywhere other than a type annotation. The reference becomes a real runtime
value (the supplied type argument), so it types as `type[T]` rather than as the `TypeVar` object.

Where a function's type argument belongs to the call, a class's belongs to the *instance*: every
instance carries the argument it was constructed with, and a method reads it through its receiver.

## a value-position type parameter is `type[T]`

```by
class C[T]:
    def kind(self) -> type[T]:
        reveal_type(T)  # revealed: type[T@C]
        return T

C[int]().kind()
```

## the instance answers with the argument it was built with

```by
class Box[T]:
    def kind(self) -> type[T]:
        return T

assert Box[int]().kind() === int
assert Box[str]().kind() === str
```

## annotation-only use is not reified

A type parameter used only in annotations stays erased, and reading it in type position gives the
`TypeVar` as before:

```by
class Box[T]:
    def __init__(self, value: T):
        self.value = value

    def get(self) -> T:
        return self.value

reveal_type(Box(1).get())  # revealed: int
```

## the constructor reads it too

An instance carries its type argument from the moment it is built, so `__init__` sees one — which is
what a generic factory needs:

```by
class Default[T]:
    value: T

    def __init__(self):
        self.value = T()

assert Default[int]().value == 0
assert Default[str]().value == ""
```

## `reified` declares reification outright

The modifier reifies a type parameter whether or not the class ever reads it as a value, so the
runtime argument is part of the declaration rather than a consequence of how the body is written:

```by
class Tagged[reified T]:
    pass

Tagged[int]()
```

## a construction says which specialization it builds

The argument has to be one the instance can carry, so a construction that solves nothing is rejected
rather than left to fail at runtime:

```by
class Box[T]:
    def kind(self) -> type[T]:
        return T

# error: [unspecialized-reified-generic] "Cannot construct reified generic class `Box` without a specialization"
Box()
```

## the declared type supplies the argument

```by
class Box[T]:
    def kind(self) -> type[T]:
        return T

b: Box[int] = Box()
assert b.kind() === int
```

## an argument solved from the constructor supplies it too

```by
class Box[T]:
    def __init__(self, value: T):
        self.value = value

    def kind(self) -> type[T]:
        return T

assert Box(1).kind() === int
```

## a reified parameter is invariant

The program can read the argument back, so two specializations of a reified class match only when
they were given the same argument — an erased parameter no member mentions is still bivariant:

```by
class Box[T]:
    def kind(self) -> type[T]:
        return T

def take(box: Box[object]) -> None: ...

# error: [invalid-argument-type]
take(Box[int]())
```

## a subclass supplies the argument

```by
class Box[T]:
    def kind(self) -> type[T]:
        return T

class IntBox(Box[int]):
    pass

assert IntBox().kind() === int
```

## a generic subclass forwards its own parameter

A class that passes its own parameter into a reified base is as reified as the base is, so its
parameter is invariant too and its constructions are solved the same way:

```by
class Box[T]:
    def kind(self) -> type[T]:
        return T

class Wrapper[U](Box[U]):
    pass

assert Wrapper[str]().kind() === str

w: Wrapper[int] = Wrapper()
assert w.kind() === int
```

## a construction of a forwarding subclass still says which specialization

```by
class Box[T]:
    def kind(self) -> type[T]:
        return T

class Wrapper[U](Box[U]):
    pass

# error: [unspecialized-reified-generic]
Wrapper()
```

## a classmethod reads through `cls`

A class method is handed the class rather than an instance, and a specialization is a class, so it
answers the same way:

```by
class Box[T]:
    @classmethod
    def kind(cls) -> type[T]:
        return T

assert Box[int].kind() === int
```

## a nested function reads the binding its method made

Everything written inside a method closes over what the method read, so depth costs nothing:

```by
class Box[T]:
    def kind(self) -> type[T]:
        def inner() -> type[T]:
            return T

        return inner()

assert Box[int]().kind() === int
```

## a variadic reifies to the tuple of its arguments

The run a variadic absorbs is spelled out as the arguments it stands for, whether the construction
writes them or the declared type supplies them — a variadic binds a *run*, not the tuple of one:

```by
class Row[T, *Rest]:
    def shape(self) -> tuple[type[T], tuple[type, ...]]:
        return (T, Rest)

assert Row[int, str, bytes]().shape() == (int, (str, bytes))

r: Row[int, str, bytes] = Row()
assert r.shape() == (int, (str, bytes))
```

## a PEP 696 default fills an unsupplied parameter

```by
class Pair[A, B = str]:
    def kinds(self) -> tuple[type[A], type[B]]:
        return (A, B)

assert Pair[int]().kinds() == (int, str)
assert Pair[int, bytes]().kinds() == (int, bytes)
```

## the class body has no instance to read from

The class body runs while the class is still being built, before any instance exists:

```by
class Box[T]:
    # error: [reified-without-receiver] "Type parameter `T` has no receiver to be read from"
    kind = T
```

## a parameter default is evaluated in the class body

```by
class Box[T]:
    # error: [reified-without-receiver]
    def kind(self, of=T) -> object:
        return of
```

## a static method is handed no receiver

```by
class Box[T]:
    @staticmethod
    def kind() -> type[T]:
        # error: [reified-without-receiver]
        return T
```

## a method written inside a block is still a method

A `def` a class body guards behind a version check has a receiver like any other:

```by
import sys

class Box[T]:
    if sys.version_info >= (3, 8):
        def kind(self) -> type[T]:
            return T

assert Box[int]().kind() === int
```

## reification decides the variance, so a declaration cannot

```by
# error: [invalid-variance-declaration] "Type parameter `T` cannot declare a variance"
class Box[out T]:
    def kind(self) -> type[T]:
        return T
```

## a class cannot answer its own subscript

`A[int]` is what records the type arguments an instance carries:

```by
class Box[T]:
    # error: [reified-without-receiver] "A reified class cannot define `__class_getitem__`"
    def __class_getitem__(cls, item: object) -> object:
        return cls

    def kind(self) -> type[T]:
        return T
```

## a `global` declaration is not a reified read

The name belongs to the module for the whole body, so reading it is reading that binding:

```by
T = "module level"

class Box[T]:
    def kind(self) -> str:
        global T
        return T

assert Box().kind() == "module level"
```

## a keyword pack has no way to be supplied

A class writes its specialization as a subscript, and a subscript takes no keyword arguments:

```by
# error: [invalid-reified-type-param] "Type parameter `Kwargs` cannot be reified"
class Row[reified **Kwargs]:
    pass
```

## a nested class of its own is not a receiver

A class written in a class body is built along with it, so its methods have no instance of the outer
class to ask:

```by
class Outer[T]:
    class Inner:
        def kind(self):
            # error: [reified-without-receiver]
            return T
```

## reading it in a `.py` file still names the `TypeVar`

Reification is basedpython's; the same class in python is the erased generic it has always been:

```toml
[environment]
python-version = "3.13"
```

`m.py`:

```py
class Box[T]:
    def kind(self):
        reveal_type(T)  # revealed: TypeVar
        return T
```
