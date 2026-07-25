# basedpython: `super` keyword

basedpython lets you write `super` and `super[T]` as if they were values, dropping the trailing `()`
/ `(pivot, owner)`. The transpile lowers them to the standard python forms:

- `super.x` → `super().x`
- `super[T].x` → `super(<MRO predecessor of T>, self).x`

In `.by` files ty resolves the same attribute via the bound super object, so type lookups follow the
MRO exactly the way the runtime does.

```toml
[environment]
python-version = "3.12"
```

## bare `super` — looks up after the enclosing class

```by
class A:
    def f(self) -> int:
        return 1

class B(A):
    def f(self) -> int:
        reveal_type(super.f())  # revealed: int
        return super.f() + 1
```

## bare `super` resolves attributes too

```by
class A:
    a: int = 1

class B(A):
    def get(self):
        reveal_type(super.a)  # revealed: int
```

## `super[T]` — pivots to the MRO entry preceding `T`

For `class C(A, B)` the MRO is `(C, A, B, object)`. `super[B]` therefore picks the entry that
precedes `B` — i.e. `A` — and starts the lookup after `A`, finding `B.f`.

```by
class A:
    def f(self) -> int:
        return 1
    a: int = 1

class B:
    def f(self) -> str:
        return "x"
    b: str = "x"

class C(A, B):
    def f(self):
        reveal_type(super.f())     # revealed: int
        reveal_type(super[A].f())  # revealed: int
        reveal_type(super[B].f())  # revealed: str
        reveal_type(super.a)       # revealed: int
        reveal_type(super[B].b)    # revealed: str
```

## attribute lookup that misses the MRO is reported

```by
class A:
    pass

class B(A):
    def f(self):
        super.does_not_exist  # error: [unresolved-attribute]
```

## `super[T]` where `T` is not in the MRO falls through

`super[Other]` in a class whose MRO does not contain `Other` produces no bound super, so the
attribute lookup falls back to lookup on the `super` class itself.

```by
class A:
    pass

class Other:
    pass

class B(A):
    def f(self):
        super[Other].x  # error: [unresolved-attribute]
```

## chained calls work the same as the desugared form

```by
class A:
    def greet(self) -> str:
        return "hi"

class B(A):
    def greet(self) -> str:
        prefix = super.greet()
        reveal_type(prefix)  # revealed: str
        return prefix + "!"
```

## `super` outside a method

`super` needs the implicit arguments a method supplies — the `__class__` cell and the receiver.
Directly in a class body there is neither, so it is reported rather than lowered (a `super()`
emitted there raises at runtime).

```by
class A:
    let a: int
        get() = 1

class B(A):
    # error: [unavailable-implicit-super-arguments]
    # error: [unresolved-attribute]
    override let a = super.a + 1
```

## `super` in a property accessor

An accessor is a method like any other, so `super` stays available there.

```by
class A:
    let a: int
        get() = 1

class B(A):
    override let a
        get() = super.a + 1

assert B().a == 2
```
