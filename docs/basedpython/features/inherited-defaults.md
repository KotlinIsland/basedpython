# inherited default values

what a method's parameters default to is part of what the method declares, so an
override that re-declares a parameter without a default of its own keeps the one
the overridden method gave it:

```by
class A:
    def f(self, a = 1): ...

class B(A):
    override def f(self, a):
        print(a)

B().f()  # prints 1
```

transpiles to:

```python
class A:
    def f(self, a = 1): ...

class B(A):
    def f(self, a=1):
        print(a)

B().f()
```

the default is written into the override's own signature, so a call that leaves
the argument out binds the same value it would have bound on the base

a default the override writes itself wins, and an argument always wins over
either

## parameters correspond the way arguments do

a parameter takes the default of the base parameter with the same name and the
same kind — positional or keyword-only:

```by
class A:
    def f(self, a = 1, *, k = "x"): ...

class B(A):
    override def f(self, a, *, k): ...

B().f()
```

a parameter the override renamed is not the same parameter, and takes nothing.
that override is reported as `invalid-method-override` anyway: a caller that
passed `a=` by keyword can no longer call it

## a default that is a value

a default reaches an override when it is a *value* — a number, a string, a
`bool`, `None`, `...`. that is exactly the set that stays a plain python default;
everything else is [re-evaluated on every call](mutable-defaults.md), in the
scope its own `def` was written in, so what it stands for is the expression
rather than any one value and there is nothing to carry:

```by
class A:
    def f(self, a = []): ...

class B(A):
    # error: [invalid-method-override] parameter `a` must have a default value
    override def f(self, a): ...
```

writing the default out is the fix. dropping a parameter's default is a
[Liskov](https://en.wikipedia.org/wiki/Liskov_substitution_principle) violation
in the first place — `B` is not usable everywhere `A` is if `B.f` demands an
argument `A.f` does not — which is what `invalid-method-override` is reporting

## inlay hints

an inherited default is written nowhere in the override, so it is shown as an
inlay hint where the override would have written it:

```by
class A:
    def f(self, a = 1, b: str = "x"): ...

class B(A):
    override def f(self, a⟨=1⟩, b: str⟨ = "x"⟩): ...
```

the spacing is the one python style asks for, so accepting a hint reads as
ordinary source. `ty.inlayHints.inheritedParameterDefaults` turns them off; see
[editor features](editor.md)

## python's own reading is unchanged

this is a basedpython rule, and writing the default into the emitted signature is
what makes it true at runtime. a `.py` file is read as python, where an override
that drops a default drops it
