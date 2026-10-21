# basedpython: type arguments a base takes from its overrides

a generic base written without type arguments is inherited at the ones the class body's overrides
need. every member the class writes overrides the one the base declares, so it has to fit the base's
declaration, and the type arguments are solved from those overrides the way a call solves a generic
function's type parameters from its arguments

## an attribute override

`x = 1` declares `x: int`, so it overrides `x: T` only at `A[int]`

```by
class A[T]:
    x: T

    def get(self) -> T:
        return self.x

class B(A):
    x = 1

reveal_type(B().get())  # revealed: int
reveal_type(B().x)  # revealed: int
```

## an annotated attribute override

an annotation says the same thing as a value does

```by
class A[T]:
    x: T

class B(A):
    x: list[str] = []

def takes(a: A[list[str]]): ...

takes(B())
```

## a method override

a method is read from what it writes itself: its annotated parameters and its annotated return type

```by
class Store[K, V]:
    def get(self, key: K) -> V:
        raise NotImplementedError

class Names(Store):
    override def get(self, key: int) -> str:
        return ""

reveal_type(Names().get(1))  # revealed: str

def takes(store: Store[int, str]): ...

takes(Names())
```

## several overrides solve together

```by
class Pair[L, R]:
    left: L

    def right(self) -> R:
        raise NotImplementedError

class P(Pair):
    left = "a"

    override def right(self) -> int:
        return 1

def takes(pair: Pair[str, int]): ...

takes(P())
```

## a type parameter no override names

nothing in the body says what `T` is, so the base needs its type argument written

```by
class A[T]:
    x: T

class B(A):  # error: [missing-type-argument]
    y = 1
```

## what an override leaves out says nothing

a parameter an override leaves unannotated takes its type from the method it overrides, and a return
type it leaves out is read off its body, which is typed in terms of the very base being solved.
neither says anything about the base, so here nothing names `T`

```by
class A[T]:
    def f(self, x: T) -> T:
        raise NotImplementedError

class B(A):  # error: [missing-type-argument]
    override def f(self, x):
        return x
```

## a type parameter with a default

a type parameter with a default does not need an override to name it

```by
class A[T, U = str]:
    x: T
    y: U

class B(A):
    x = 1

def takes(a: A[int, str]): ...

takes(B())
```

## an override solves a type parameter with a default too

an override that names a type parameter decides it whether or not the parameter has a default, the
same way a call's arguments decide a type parameter that has one

```by
class C[U = str]:
    y: U

class D(C):
    y = 2

def takes(c: C[int]): ...

takes(D())
```

## overrides that disagree

an attribute that can be assigned through the base is invariant: a subclass that narrowed it would
let a write through the base store a value the subclass rules out. so `x = 1` asks for exactly
`A[int]`, and a `get` returning `str` rules that out. no specialization fits both, so the base needs
its type argument written

```by
class A[T]:
    x: T

    def get(self) -> T:
        return self.x

class B(A):  # error: [missing-type-argument]
    x = 1

    override def get(self) -> str:
        return ""
```

## a solution the class header cannot name

the transpiled class writes the arguments into its header, which sees the module and the class's own
type parameters and nothing else. an override typed by a method's `Self`, by a method's own type
parameter or by an anonymous `some` parameter solves the base to something the header cannot name,
so the base needs its type arguments written

```by
from typing import Self

class A[T]:
    def f(self) -> T:
        raise NotImplementedError

class SelfTyped(A):  # error: [missing-type-argument]
    override def f(self) -> Self:
        return self

class B[K, V]:
    def get(self, key: K) -> V:
        raise NotImplementedError

class HoleTyped(B):  # error: [missing-type-argument]
    override def get(self, key: some int) -> str:
        return ""
```

## a generic subclass passes its own type parameter on

the class's own type parameters are in scope in its header, so an override typed by one of them
solves the base to it

```by
class A[T]:
    x: T

    def get(self) -> T:
        return self.x

class H[U](A):
    x: U

def check(h: H[int]):
    reveal_type(h.get())  # revealed: int
```

## an override that returns the class itself

`IntNode`'s override returns an `IntNode`, which is a `Node[T]` only once `IntNode`'s base is known,
and that base is what is being solved. it is not solved from itself, so the base needs its type
argument written

```by
class Node[T]:
    def child(self) -> Node[T]:
        raise NotImplementedError

class IntNode(Node):  # error: [missing-type-argument]
    override def child(self) -> IntNode:
        return self
```

## a member under a visibility keyword

a `private` or `protected` member is emitted under a name of its own, so it does not override the
base's member of the same name and says nothing about the base

```by
class A[T]:
    x: T

class B(A):  # error: [missing-type-argument]
    private x: int = 1  # error: [invalid-override-visibility]
```

## a variadic type parameter

a `*Ts` is solved to a run of types, which the header writes as that many arguments

```by
class Fixed[*Ts]:
    def f(self) -> tuple[*Ts]:
        raise NotImplementedError

class Two(Fixed):
    override def f(self) -> tuple[int, str]:
        return (1, "")

def takes(fixed: Fixed[int, str]): ...

takes(Two())
```

## a base that cannot be subscripted as the class is made

a stub can call a class generic that is not subscriptable at runtime: typeshed's `map` is generic,
but `map[int]` raises. the arguments would have to be written in the transpiled header, so they are
not inferred for such a base

```by
class M(map):  # error: [missing-type-argument]
    override def __next__(self) -> int:
        return 1
```

## a written type argument is used as written

```by
class A[T]:
    x: T

class B(A[object]):
    x = 1

def takes(a: A[object]): ...

takes(B())
```

## only basedpython infers it

```toml
[environment]
python-version = "3.12"
```

a `.py` file keeps python's meaning: a base written without type arguments is inherited at its
defaults, which is `Unknown` for a type parameter with none

```py
class A[T]:
    x: T

    def get(self) -> T:
        return self.x

class B(A):  # error: [missing-type-argument]
    x = 1

reveal_type(B().get())  # revealed: Unknown
```
