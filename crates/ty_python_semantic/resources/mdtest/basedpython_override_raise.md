# basedpython: overrides may not raise more than what they override

A call is checked against the type it can see. When a base-class method cannot raise, nothing at a
call on the base type says an exception can escape — yet a subclass substituted for it can still
raise from that call.

`override-raise` closes that hole by making a base method's exception set bound every override of
it. It is a strictness option and off by default, since honouring it makes the base's set part of
its contract.

```toml
[environment]
python-version = "3.12"

[rules]
override-raise = "error"
```

## an override may not introduce an exception

```by
def a() -> A:
    return B()

class A:
    def foo(self):
        pass

class B(A):
    # error: [override-raise] "`foo` can raise `TypeError`, which the method it overrides cannot"
    override def foo(self):
        raise TypeError

def main():
    a().foo()
```

## an override raising what the base already raises is fine

```by
class A:
    def foo(self) raises TypeError: ...

class B(A):
    override def foo(self):
        raise TypeError
```

## an override may raise less

```by
class A:
    def foo(self) raises TypeError | ValueError: ...

class B(A):
    override def foo(self):
        raise TypeError

class C(A):
    override def foo(self):
        pass
```

## only the part outside the base's set is reported

```by
class A:
    def foo(self) raises TypeError: ...

class B(A):
    # error: [override-raise] "`foo` can raise `ValueError`, which the method it overrides cannot"
    override def foo(self):
        if True:
            raise TypeError
        raise ValueError
```

## a gradual base opts its overrides out

```by
class A:
    def foo(self) raises ...: ...

class B(A):
    override def foo(self):
        raise TypeError
```

## a gradual override is not reported either

An override that declares it may raise anything has opted out of tracking, exactly as anywhere else.

```by
class A:
    def foo(self): ...

class B(A):
    override def foo(self) raises ...:
        raise TypeError
```

## the exception is traced through calls

```by
def boom():
    raise TypeError

class A:
    def foo(self): ...

class B(A):
    # error: [override-raise] "`foo` can raise `TypeError`, which the method it overrides cannot"
    override def foo(self):
        boom()
```

## only the nearest defining superclass is blamed

`C` matches what `B` declares, so `C` is not reported for a bound `B` already violates.

```by
class A:
    def foo(self): ...

class B(A):
    # error: [override-raise] "`foo` can raise `TypeError`, which the method it overrides cannot"
    override def foo(self) raises TypeError:
        raise TypeError

class C(B):
    override def foo(self) raises TypeError:
        raise TypeError
```

## a constructor is not checked

`ty` exempts constructors from override compatibility, and this follows that.

```by
class A:
    def __init__(self): ...

class B(A):
    override def __init__(self):
        raise TypeError
```
