# `super` keyword

basedpython lets you reference `super` like a name instead of a call:

```by
class B(A):
    def f(self):
        super.greet()
```

transpiles to:

```python
class B(A):
    def f(self):
        super().greet()
```

## the bare form — `super.x`

`super.x` desugars to `super().x`. python's zero-arg `super()` requires a
method context (it pulls the enclosing class from the `__class__` cell), so
`super.x` outside a method is a runtime error — exactly like the python it
lowers to

the receiver is read where `super` is written, as `super()` reads it: the first
parameter of the function or lambda around it. a lambda passes its own first
argument, not the method's, and a lambda with no parameter, or a comprehension,
has no receiver to pass and is reported:

```by
class B(A):
    def f(self):
        g = lambda other: super.greet()  # `other` is the receiver
```

the keyword is the base of an attribute access, and it is the builtin `super`
there even where the module binds the name to something else. read anywhere
else, `super` is the name as python reads it:

```by
class B(A):
    def f(self, super: int):
        super.greet(super)  # the builtin's `greet`, passed the parameter
```

## assigning through `super`

a `super()` object has no attributes of its own, and does not pass an
assignment or a deletion on to the class it reads past, so python raises
`AttributeError` for `super().x = v` and `del super().x` whatever `x` is. the
keyword is that object, so `super.x = v`, `super.x += v` and `del super.x` are
reported. assign through the receiver instead:

```by
class B(A):
    def f(self):
        self.x = 1  # not `super.x = 1`
```

## targeted form — `super[T].x`

`super[T].x` resolves `x` against the MRO entry that **precedes** `T` in the
enclosing class' MRO. like the bare form, `super[T]` is the keyword only as the
base of an attribute access; anywhere else it is a subscript of the name

example:

```by
class A:
    def f(self): ...

class B:
    def f(self): ...

class C(A, B):
    def f(self):
        super[B].f()  # → super(A, self).f()
```

C's MRO is `(C, A, B, object)`; the entry preceding `B` is `A`, so the bound
super starts the search after `A` and lands on `B.f`

`T` has to be a class in that MRO other than the enclosing class itself, which
has nothing before it. anything else is reported, since the lookup would raise
`ValueError` when it runs — so is a value of a `type[...]` type, which may be a
subclass the MRO does not hold:

```by
class C(A):
    def f(self):
        super[B].f()  # error: `B` is not in the MRO of `C`
```
