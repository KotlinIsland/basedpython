# basedpython: safe covariant private members

Privacy is what makes variance safe. A private member is invisible outside its class, so it cannot
be used to tell two specializations apart — which is what lets a covariant class hold a mutable
field of its type parameter. The flip side is that a widened view of the class learns nothing about
that member: it keeps its declared type instead of picking up the receiver's arguments, and through
any receiver but the class's own — anything that is not the `self` the body was handed — that type
is erased to what such a view knows. A read gets back `T`'s bound, and a write is accepted from
nothing at all.

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

## a write through a widened view is accepted from nothing

Storage is invariant in its own type, so a write has to be valid for every type the erased member
could really have. Nothing is, which is why the write type is `Never`.

Privacy is what exempts `t` from constraining variance; the public `g` below still consumes a `T`,
so the class is separately reported for not honouring its own `out`.

```by
# error: [invalid-generic-class] "Variance of type variable `T` is incompatible with its usage in `A`"
class A[out T]:
    private t: T

    def f(self, other: A[object]):
        # error: [invalid-assignment] "Object of type `1` is not assignable to attribute `t` of type `Never`"
        other.t = 1

    def g(self, other: A[object], t: T):
        # a real `T` gets no further: it is `self`'s parameter, and says nothing about the one
        # `other` is hiding
        # error: [invalid-assignment] "Object of type `T@A` is not assignable to attribute `t` of type `Never`"
        other.t = t
```

## …including through a receiver the body constructed itself

`a` below is an `A[object]` and its storage has nothing to do with `self`'s `T`.

```by
# error: [invalid-generic-class] "Variance of type variable `T` is incompatible with its usage in `A`"
class A[out T]:
    private t: T

    def f(self, t: T):
        a = A[object]()
        # error: [invalid-assignment] "Object of type `1` is not assignable to attribute `t` of type `Never`"
        a.t = 1
        # error: [invalid-assignment] "Object of type `T@A` is not assignable to attribute `t` of type `Never`"
        a.t = t
```

## the class's own receiver is `self`, however it is annotated

An explicitly widened `self` is still the receiver the call site had, and the class's type
parameters are that receiver's — so the member keeps its declared type, un-erased, and a real `T`
can be written to it. This holds through a capture in a nested function too.

```by
# error: [invalid-generic-class] "Variance of type variable `T` is incompatible with its usage in `A`"
class A[out T]:
    private t: T

    def f(self: A[object], t: T):
        reveal_type(self.t)  # revealed: T@A
        self.t = t
        # error: [invalid-assignment] "Object of type `"asdf"` is not assignable to attribute `t` of type `T@A`"
        self.t = "asdf"

    def g(self: A[object], t: T):
        def inner():
            self.t = t
```

## an unspecialized construction is a widened view too

`A()` is an `A[Unknown]`, not the `A[T]` whose body we are inside.

```by
class A[T]:
    private t: T

    def f(self):
        # error: [invalid-assignment] "Object of type `1` is not assignable to attribute `t` of type `Never`"
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

A public mutable attribute is exactly what the erasure rule does *not* cover, so it pins `A` to
invariance and the `out` declaration is reported.

```by
# error: [invalid-generic-class] "Variance of type variable `T` is incompatible with its usage in `A`"
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
        # error: [invalid-assignment] "Object of type `1` is not assignable to attribute `t` of type `Never`"
        other.t = 1
```

## a nested occurrence is erased soundly

`list` is invariant, so the erasure has to be a materialization rather than a naive substitution of
the bound. Neither materialization can say more than "some list" about an invariant occurrence, so
the erased element stays gradual and a write is neither precise nor rejected.

```by
class A[T]:
    private items: list[T]

def f(a: A[int]):
    reveal_type(a.items)  # revealed: list[*]
    a.items = [1]
```

## a union of two specializations is a widened view of each

A write to a union has to be valid for every element, and no element accepts anything. The public
`produce` is what keeps the two specializations from collapsing into each other: without it `T` is
bivariant and `A[int] | A[str]` is just `A[int]`.

```by
class A[out T]:
    private t: T

    def produce(self) -> T:
        raise NotImplementedError

def f(a: A[int] | A[str]):
    # error: [invalid-assignment] "Object of type `1` is not assignable to attribute `t` on type `A[int] | A[str]`"
    a.t = 1
```
