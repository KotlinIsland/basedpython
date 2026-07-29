# basedpython: safe covariant private members

Privacy is what makes variance safe. A private member is invisible outside its class, so it cannot
be used to tell two specializations apart — which is what lets a covariant class hold a mutable
field of its type parameter. The flip side is that a widened view of the class learns nothing about
that member: through any receiver but the class's own it keeps its declared type instead of picking
up the receiver's arguments, so a write must supply a real `T` and a read gets back only `T`'s
bound.

```toml
[environment]
python-version = "3.12"
```

## the class's own view is unaffected

```by
class A[out T]:
    private t: T

    def f(self) -> T:
        reveal_type(self.t)  # revealed: T@A
        return self.t

    def g(self, other: Self):
        self.t = other.t
```

## a read through a widened view is erased to the bound

The receiver's own type argument says nothing about what the object really holds — that is what
being widened means.

```by
class A[T]:
    private t: T

    def f(self):
        a1 = A[int]()
        reveal_type(a1.t)  # revealed: object
        print(a1.t)
```

## …which is what keeps a contravariant read honest

`A[object]` is an `A[int]` under `in T`, so an `A[int]` may really be holding a `str`. The erased
read is the only answer that is not a lie.

```by
class A[in T]:
    private t: T

    def f(self):
        a1 = A[object]()
        a2: A[int] = a1
        # error: [unsupported-operator] "Operator `+` is not supported between objects of type `object` and `1`"
        print(a2.t + 1)
```

## …and stops the value being funnelled back into `T`

```by
class A[out T]:
    private t: T

    def f(self, other: A[object]):
        # error: [invalid-assignment] "Object of type `object` is not assignable to attribute `t` of type `T@A`"
        self.t = other.t
```

## a write through a widened view must supply a real `T`

```by
class A[out T]:
    private t: T

    def f(self: A[object], t: T):
        self.t = t

    def g(self: A[object]):
        # error: [invalid-assignment] "Object of type `"asdf"` is not assignable to attribute `t` of type `T@A`"
        self.t = "asdf"

    def h(self, other: A[object]):
        # error: [invalid-assignment] "Object of type `1` is not assignable to attribute `t` of type `T@A`"
        other.t = 1
```

## an unspecialized construction is a widened view too

`A()` is an `A[Unknown]`, not the `A[T]` whose body we are inside.

```by
class A[T]:
    private t: T

    def f(self):
        # error: [invalid-assignment] "Object of type `1` is not assignable to attribute `t` of type `T@A`"
        A().t = 1
```

## an invariant type parameter specializes as usual

No specialization of an invariant class is assignable to another, so every receiver's argument is
exact and ordinary specialization is already sound.

```by
class A[T]:
    private t: T

    # a public member that both reads and writes `T` forces invariance
    def swap(self, t: T) -> T:
        old = self.t
        self.t = t
        return old

    def f(self):
        a1 = A[int]()
        reveal_type(a1.t)  # revealed: int
        a1.t = 1
```

## a private member that does not mention the type parameter is unaffected

```by
class A[T]:
    private n: int
    t: T

    def f(self, other: A[object]):
        reveal_type(other.n)  # revealed: int
        other.n = 1
```

## a public member is unaffected

```by
class A[out T]:
    t: T

    def f(self, other: A[object]):
        reveal_type(other.t)  # revealed: object
```

## the underscore spelling is private too

Privacy is read off the member's name as well as the `private` keyword: a leading underscore is
private, and so is a name-mangled `__t`.

```py
class A[T]:
    _t: T

def f(a: A[object]):
    reveal_type(a._t)  # revealed: object
```

## a dunder is not private

A dunder is part of the public protocol surface, so it constrains variance in the usual way and
specializes through any view of the class.

```py
class B[T]:
    __t__: T

def f(b: B[int]):
    reveal_type(b.__t__)  # revealed: int
```

## a `private` method is private too

The keyword parses as a synthetic decorator rather than an annotation, so its privacy rides on the
function instead of on the declaration — but it means the same thing. Erasing the receiver's
argument leaves a callback nothing can be passed to.

```by
class A[T]:
    private def consume(self, t: T): ...

def f(a: A[int]):
    # error: [invalid-argument-type] "Argument to bound method `A.consume` is incorrect: Expected `Never`, found `1`"
    a.consume(1)
```

## a private producer stays callable through a widened view

```by
class A[T]:
    private def produce(self) -> T:
        raise NotImplementedError

def f(a: A[int]):
    reveal_type(a.produce())  # revealed: object
```

## a `__getattr__` result is not a declared member

Whatever `__getattr__` answers with, it is not a member the class declared, so it is never private —
however its name is spelled.

```py
class A[T]:
    def __getattr__(self, name: str) -> T:
        raise AttributeError(name)

def f(a: A[int]):
    reveal_type(a._anything)  # revealed: int
```

## a non-generic class is unaffected

```py
class A:
    _t: int

def f(a: A):
    reveal_type(a._t)  # revealed: int
    a._t = 1
```

## a subclass that pins the type argument

`B` is an `A[int]` and nothing else, so there is no differently-specialized view of `B` to reach its
`t` through.

```by
class A[T]:
    private t: T

class B(A[int]):
    def f(self):
        reveal_type(self.t)  # revealed: int
        self.t = 1
```

## `bivariant-private-attributes` does not turn the rule off

With the option off a private attribute falls back to immutable-but-readable, which is covariant —
still not invariant, so a widened view still knows nothing about it.

```toml
[environment]
python-version = "3.12"

[analysis]
bivariant-private-attributes = false
```

```by
class A[T]:
    private t: T

    def g(self: A[object]):
        # error: [invalid-assignment] "Object of type `"asdf"` is not assignable to attribute `t` of type `T@A`"
        self.t = "asdf"
```

## a subclass that stays generic carries the constraint forward

```by
class A[T]:
    private t: T

class C[U](A[U]):
    def f(self, other: C[object]):
        # error: [invalid-assignment] "Object of type `1` is not assignable to attribute `t` of type `U@C`"
        other.t = 1
```

## a nested occurrence is erased soundly

`list` is invariant, so the erasure has to be the top materialization rather than a naive
substitution of the bound.

```by
class A[T]:
    private items: list[T]

def f(a: A[int]):
    reveal_type(a.items)  # revealed: list[*]
```
