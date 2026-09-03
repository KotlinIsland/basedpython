# basedpython: a method's defaults reach its overrides

what a method's parameters default to is part of what the method declares, alongside their types. so
an override that re-declares a parameter without a default of its own keeps the one the overridden
method gave it, and a call may still leave that argument out.

```toml
[environment]
python-version = "3.12"
```

## a default reaches the override

`B.f` says nothing about what `a` defaults to, so it defaults to what `A.f` says, and the call may
leave it out.

```by
class A:
    def f(self, a = 1): ...

class B(A):
    override def f(self, a): ...

B().f()
B().f(2)
```

## a keyword-only default

a parameter is matched to the base's by name and by whether it is positional or keyword-only, the
same way an argument is matched to a parameter.

```by
class A:
    def f(self, *, a = "x"): ...

class B(A):
    override def f(self, *, a): ...

B().f()
```

## a positional-only default

```by
class A:
    def f(self, a = 1, /): ...

class B(A):
    override def f(self, a, /): ...

B().f()
```

## a default carried down a chain of overrides

each override declares the default it inherited, so the next one down inherits it in turn.

```by
class A:
    def f(self, a = 1): ...

class B(A):
    override def f(self, a): ...

class C(B):
    override def f(self, a): ...

C().f()
```

## a parameter the base does not default stays required

```by
class A:
    def f(self, a, b = 1): ...

class B(A):
    override def f(self, a, b): ...

# error: [missing-argument] "No argument provided for required parameter `a`"
B().f()
```

## a parameter the override renamed takes nothing

a renamed parameter is not the same parameter — a caller that passed `a=` by keyword can no longer
call the override at all, which `invalid-method-override` reports.

```by
class A:
    def f(self, a = 1): ...

class B(A):
    # error: [invalid-method-override] "Definition is incompatible with `A.f`"
    override def f(self, b): ...

# error: [missing-argument] "No argument provided for required parameter `b`"
B().f()
```

## a default that is an expression rather than a value

basedpython re-evaluates a non-scalar default on every call, so what such a default stands for is
the expression, which runs in the scope its own `def` was written in. there is no value to carry to
the override, and the parameter stays required.

```by
class A:
    def f(self, a = []): ...

class B(A):
    # error: [invalid-method-override] "Definition is incompatible with `A.f`"
    override def f(self, a): ...
```

## a value python has no literal for

`1e400` overflows to an infinity, which is a value — but not one that can be written back into a
signature, since python spells it with a name (`math.inf`) rather than a literal. so it is not
carried either.

```by
class A:
    def f(self, a = 1e400): ...

class B(A):
    # error: [invalid-method-override] "Definition is incompatible with `A.f`"
    override def f(self, a): ...
```

## a default reaching across modules

`base.by`:

```by
class A:
    def f(self, a = 1, b = None): ...
```

`main.by`:

```by
from base import A

class B(A):
    override def f(self, a, b): ...

B().f()
```

## python carries nothing

this is a basedpython rule, and the lowering to python is what makes it true at runtime. a `.py`
file gets python's own reading, where an override that drops a default drops it.

```py
class A:
    def f(self, a=1): ...

class B(A):
    # error: [invalid-method-override] "Definition is incompatible with `A.f`"
    def f(self, a): ...

# error: [missing-argument] "No argument provided for required parameter `a`"
B().f()
```

## the base's own defaults are untouched

an override taking a default does not give one to a parameter of the base that never had one.

```by
class A:
    def f(self, a): ...

class B(A):
    override def f(self, a): ...

# error: [missing-argument] "No argument provided for required parameter `a`"
A().f()
```
