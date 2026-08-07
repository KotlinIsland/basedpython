<!-- WARNING: This file is auto-generated (cargo dev generate-all). Edit the lint-declarations in 'crates/ty_python_semantic/src/types/diagnostic.rs' if you want to change anything here. -->

# Rules

## `abstract-method-in-final-class`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.13">0.0.13</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22abstract-method-in-final-class%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2248" target="_blank">View source</a>
</small>


**What it does**


Checks for `@final` classes that have unimplemented abstract methods.

**Why is this bad?**


A class decorated with `@final` cannot be subclassed. If such a class has abstract
methods that are not implemented, the class can never be properly instantiated, as
the abstract methods can never be implemented (since subclassing is prohibited).

At runtime, instantiation of classes with unimplemented abstract methods is only
prevented for classes that have `ABCMeta` (or a subclass of it) as their metaclass.
However, type checkers also enforce this for classes that do not use `ABCMeta`, since
the intent for the class to be abstract is clear from the use of `@abstractmethod`.

**Example**


```python
from abc import ABC, abstractmethod
from typing import final


class Base(ABC):
    @abstractmethod
    def method(self) -> int: ...


@final
# `Derived` does not implement `method`
class Derived(Base):  # error
    pass
```

## `ambiguous-context-argument`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.61">0.0.61</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22ambiguous-context-argument%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1945" target="_blank">View source</a>
</small>


**What it does**

Checks for calls where several `context` declarations in the same scope
could fill one `context` parameter.

**Why is this bad?**

The implicit argument is chosen by assignability, not by name. When two
declarations in the winning scope both match, either choice would be
arbitrary — the call must pass the argument explicitly (or the extra
declaration must move to another scope).

**Examples**

```python
def f(a: int, context b: str): ...

context s1 = "hello"
context s2 = "world"
f(1)          # error: `s1` and `s2` both match
f(1, b=s1)    # ok — explicit
```

## `ambiguous-conversion`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.39">0.0.1-alpha.39</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22ambiguous-conversion%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1158" target="_blank">View source</a>
</small>


**What it does**

Checks for conversion sites where more than one conversion applies — two
dunders, a dunder and an in-scope `implementation`, or two applicable
`implementation`s of the same interface and type.

**Why is this bad?**

`__from__` and `__into__` are hand-written bodies that can disagree, so
which one runs must not depend on arbitrary ordering. Remove one of them,
or write the conversion you want explicitly.

**Example**


```by
class Celsius:
    def __into__(self) -> Fahrenheit: ...

class Fahrenheit:
    @classmethod
    def __from__(cls, value: Celsius) -> Self: ...

report(Celsius())  # error: two conversions apply
```

## `ambiguous-extension-member`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.3">0.0.1-alpha.3</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22ambiguous-extension-member%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1070" target="_blank">View source</a>
</small>


**What it does**

Checks for attribute accesses that resolve to a member supplied by more
than one applicable basedpython extension.

**Why is this bad?**

When two extensions in scope both add the same member to the receiver's
type, the access is ambiguous — which implementation runs would depend
on arbitrary ordering. Constrain one of the extensions (or drop the
import that brings the second into scope) so exactly one applies.

**Example**


```by
extension list:
    def second(self) -> Element: ...

extension list:
    def second(self) -> Element: ...

[1, 2].second()  # error: ambiguous extension member
```

## `ambiguous-protocol-member`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.20">0.0.1-alpha.20</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22ambiguous-protocol-member%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L388" target="_blank">View source</a>
</small>


**What it does**


Checks for protocol classes with members that will lead to ambiguous interfaces.

**Why is this bad?**


Assigning to an undeclared variable in a protocol class, or to an undeclared attribute
through a protocol method's `self` or `cls` receiver, leads to an ambiguous interface
which may lead to the type checker inferring unexpected things. It's recommended to
ensure that all members of a protocol class are explicitly declared.

**Examples**


```py
from typing import ClassVar, Protocol


class BaseProto(Protocol):
    a: int  # fine (explicitly declared as `int`)
    instance_member: str
    class_member: ClassVar[str]

    # fine: a method definition using `def` is considered a declaration
    def method_member(self) -> int: ...

    def method(self) -> None:
        self.instance_member = "value"  # fine (declared in the class body)
        self.implicit = "value"  # error: [ambiguous-protocol-member]

    @classmethod
    def class_method(cls) -> None:
        cls.class_member = "value"  # fine (declared in the class body)
        cls.implicit_class = "value"  # error: [ambiguous-protocol-member]

    # no explicit declaration, leading to ambiguity
    c = "some variable"  # error
    # no explicit declaration, leading to ambiguity
    b = method_member  # error

    # This creates implicit assignments of `d` and `e` in the protocol class body.
    # Were they really meant to be considered protocol members?
    # error: "`d` is not declared as a protocol member"
    # error: "`e` is not declared as a protocol member"
    for d, e in enumerate(range(42)):
        pass


class SubProto(BaseProto, Protocol):
    a = 42  # fine (declared in superclass)
```

## `assert-type-unspellable-subtype`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.14">0.0.14</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22assert-type-unspellable-subtype%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2293" target="_blank">View source</a>
</small>


**What it does**


Checks for `assert_type()` calls where the actual type
is an unspellable subtype of the asserted type.

**Why is this bad?**


`assert_type()` is intended to ensure that the inferred type of a value
is exactly the same as the asserted type. But in some situations, ty
has nonstandard extensions to the type system that allow it to infer
more precise types than can be expressed in user annotations. ty emits a
different error code to [`type-assertion-failure`](#type-assertion-failure) in these situations so
that users can easily differentiate between the two cases.

**Example**


```toml
[environment]
python-version = "3.11"
```

```python
from typing import assert_type


def _(x: int):
    assert_type(x, int)  # fine
    if x:
        # the actual type is `int & ~AlwaysFalsy`,
        # which excludes types like `Literal[0]`
        # error: [assert-type-unspellable-subtype]
        assert_type(x, int)
```

## `blanket-ignore-comment`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'ignore'."><code>ignore</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.57">0.0.57</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22blanket-ignore-comment%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Fsuppression.rs#L64" target="_blank">View source</a>
</small>


**What it does**


Checks for `ty: ignore` comments that don't specify which rules to ignore.

**Why is this bad?**


A blanket `ty: ignore` comment suppresses every type-checking diagnostic on the
applicable line or file. Specifying rule codes documents which diagnostics are
expected and prevents the comment from silencing unrelated errors.

**Examples**


```py
# error
value = unknown  # ty: ignore
```

Use instead:

```py
value = unknown  # ty: ignore[unresolved-reference]
```

## `bool-as-int`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.61">0.0.61</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22bool-as-int%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1883" target="_blank">View source</a>
</small>


**What it does**

Checks for a `bool` value in a position that expects a number, where it is
admitted only because `bool` is a subclass of `int`.

**Why is this bad?**

Nothing is converted here — `bool` really is a subclass of `int`, and `True`
and `False` really are `1` and `0`. That is the problem: the value satisfies
an `int` (or `float`, or `complex`) annotation silently, so a boolean that
reached a numeric slot by mistake type-checks exactly like one that was meant
to. Writing `int(...)` says the number is what you meant, and widening the
annotation to `bool` says the flag is.

The value has to be a boolean and the target a number for this to fire, so
arithmetic on booleans, a `bool` annotation, and a container of booleans are
all left alone. Note that `int | bool` is not an escape hatch: a union of a
class and its subclass simplifies to the supertype, so that annotation *is*
`int` and is reported as such.

**Examples**

```python
def take(n: int): ...

a: int = True       # warning: `bool` used as `int`
take(True)          # warning: `bool` used as `int`

a2: int = int(True) # ok — explicit
a3: bool = True     # ok
a4 = True + 1       # ok — a boolean used as a boolean
```

## `call-abstract-method`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.16">0.0.16</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22call-abstract-method%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2257" target="_blank">View source</a>
</small>


**What it does**


Checks for calls to abstract `@classmethod`s or `@staticmethod`s
with "trivial bodies" when accessed on the class object itself.

"Trivial bodies" are bodies that solely consist of `...`, `pass`,
a docstring, and/or `raise NotImplementedError`.

**Why is this bad?**


An abstract method with a trivial body has no concrete implementation
to execute, so calling such a method directly on the class will probably
not have the desired effect.

It is also unsound to call these methods directly on the class. Unlike
other methods, ty permits abstract methods with trivial bodies to have
non-`None` return types even though they always return `None` at runtime.
This is because it is expected that these methods will always be
overridden rather than being called directly. As a result of this
exception to the normal rule, ty may infer an incorrect type if one of
these methods is called directly, which may then mean that type errors
elsewhere in your code go undetected by ty.

Calling abstract classmethods or staticmethods via `type[X]` is allowed,
since the actual runtime type could be a concrete subclass with an implementation.

**Example**


```python
from abc import ABC, abstractmethod


class Foo(ABC):
    @classmethod
    @abstractmethod
    def method(cls) -> int: ...


# cannot call abstract classmethod
Foo.method()  # error
```

## `call-non-callable`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22call-non-callable%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L242" target="_blank">View source</a>
</small>


**What it does**


Checks for calls to non-callable objects.

**Why is this bad?**


Calling a non-callable object will raise a `TypeError` at runtime.

**Examples**


```python
# TypeError: 'int' object is not callable
4()  # error
```

## `call-top-callable`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.7">0.0.7</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22call-top-callable%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L251" target="_blank">View source</a>
</small>


**What it does**


Checks for calls to objects typed as `Top[Callable[..., T]]` (the infinite union of all
callable types with return type `T`).

**Why is this bad?**


When an object is narrowed to `Top[Callable[..., object]]` (e.g., via `callable(x)` or
`isinstance(x, Callable)`), we know the object is callable, but we don't know its
precise signature. This type represents the set of all possible callable types
(including, e.g., functions that take no arguments and functions that require arguments),
so no specific set of arguments can be guaranteed to be valid.

**Examples**


```python
def f(x: object):
    if callable(x):
        # We know `x` is callable, but not what arguments it accepts
        x()  # error
```

## `conflicting-declarations`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22conflicting-declarations%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L269" target="_blank">View source</a>
</small>


**What it does**


Checks whether a variable has been declared as two conflicting types.

**Why is this bad**


A variable with two conflicting declarations likely indicates a mistake.
Moreover, it could lead to incorrect or ill-defined type inference for
other code that relies on these variables.

**Examples**


```python
if __name__ == "__main__":
    a: int
else:
    a: str

a = 1  # error
```

## `conflicting-metaclass`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22conflicting-metaclass%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L278" target="_blank">View source</a>
</small>


**What it does**


Checks for class definitions where the metaclass of the class
being created would not be a subclass of the metaclasses of
all the class's bases.

**Why is it bad?**


Such a class definition raises a `TypeError` at runtime.

**Examples**


```pyi
class M1(type): ...
class M2(type): ...
class A(metaclass=M1): ...
class B(metaclass=M2): ...

# TypeError: metaclass conflict
class C(A, B): ...  # error
```

## `cyclic-class-definition`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22cyclic-class-definition%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L287" target="_blank">View source</a>
</small>


**What it does**


Checks for class definitions in stub files that inherit
(directly or indirectly) from themselves.

**Why is it bad?**


Although forward references are natively supported in stub files,
inheritance cycles are still disallowed, as it is impossible to
resolve a consistent [method resolution order] for a class that
inherits from itself.

**Examples**


`foo.pyi`:

```pyi
class A(B): ...  # error
class B(A): ...  # error
```

[method resolution order]: https://docs.python.org/3/glossary.html#term-method-resolution-order

## `cyclic-type-alias-definition`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.29">0.0.1-alpha.29</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22cyclic-type-alias-definition%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L296" target="_blank">View source</a>
</small>


**What it does**


Checks for type alias definitions that (directly or mutually) refer to themselves.

**Why is it bad?**


Although it is permitted to define a recursive type alias, it is not meaningful
to have a type alias whose expansion can only result in itself, and is therefore not allowed.

**Examples**


```toml
[environment]
python-version = "3.12"
```

```python
type Itself = Itself  # error

type A = B  # error
type B = A  # error
```

## `dataclass-field-order`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.15">0.0.15</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22dataclass-field-order%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L341" target="_blank">View source</a>
</small>


**What it does**


Checks for dataclass definitions where required fields are defined after
fields with default values.

**Why is this bad?**


In dataclasses, all required fields (fields without default values) must be
defined before fields with default values. This is a Python requirement that
will raise a `TypeError` at runtime if violated.

**Example**


```python
from dataclasses import dataclass


@dataclass
class Example:
    x: int = 1  # Field with default value
    # Required field after field with default
    y: str  # error
```

## `deprecated`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.16">0.0.1-alpha.16</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22deprecated%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L314" target="_blank">View source</a>
</small>


**What it does**


Checks for uses of deprecated items

**Why is this bad?**


Deprecated items should no longer be used.

**Examples**


```toml
[environment]
python-version = "3.13"
```

```python
import warnings


@warnings.deprecated("use new_func instead")
def old_func(): ...


old_func()  # error: [deprecated]
```

## `division-by-zero`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'ignore'."><code>ignore</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22division-by-zero%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L305" target="_blank">View source</a>
</small>


**What it does**


It detects division by zero.

**Why is this bad?**


Dividing by zero raises a `ZeroDivisionError` at runtime.

**Rule status**


This rule is currently disabled by default because of the number of
false positives it can produce.

**Examples**


```python
5 / 0  # error
```

## `duplicate-base`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22duplicate-base%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L323" target="_blank">View source</a>
</small>


**What it does**


Checks for class definitions with duplicate bases.

**Why is this bad?**


Class definitions with duplicate bases raise `TypeError` at runtime.

**Examples**


```python
class A: ...


# TypeError: duplicate base class
class B(A, A): ...  # error
```

## `duplicate-kw-only`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.12">0.0.1-alpha.12</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22duplicate-kw-only%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L332" target="_blank">View source</a>
</small>


**What it does**


Checks for dataclass definitions with more than one field
annotated with `KW_ONLY`.

**Why is this bad?**


`dataclasses.KW_ONLY` is a special marker used to
emulate the `*` syntax in normal signatures.
It can only be used once per dataclass.

Attempting to annotate two different fields with
it will lead to a runtime error.

**Examples**


```python
from dataclasses import dataclass, KW_ONLY


# Crash at runtime
@dataclass
class A:  # error
    b: int
    _1: KW_ONLY
    c: str
    _2: KW_ONLY
    d: bytes
```

## `empty-body`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.14">0.0.14</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22empty-body%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L488" target="_blank">View source</a>
</small>


**What it does**


Detects functions with empty bodies that have a non-`None` return type annotation.

The errors reported by this rule have the same motivation as the [`invalid-return-type`](#invalid-return-type)
rule. The diagnostic exists as a separate error code to allow users to disable this
rule while prototyping code. While we strongly recommend enabling this rule if
possible, users migrating from other type checkers may also find it useful to
temporarily disable this rule on some or all of their codebase if they find it
results in a large number of diagnostics.

**Why is this bad?**


A function with an empty body (containing only `...`, `pass`, or a docstring) will
implicitly return `None` at runtime. Returning `None` when the return type is non-`None`
is unsound, and will lead to ty inferring incorrect types elsewhere.

Functions with empty bodies are permitted in certain contexts where they serve as
declarations rather than implementations:

- Functions in stub files (`.pyi`)
- Methods in Protocol classes
- Abstract methods decorated with `@abstractmethod`
- Overload declarations decorated with `@overload`
- Functions in `if TYPE_CHECKING` blocks

**Examples**


```python
def foo() -> int: ...  # error: [empty-body]


def bar() -> str:  # error: [empty-body]
    """A function that does nothing."""
    pass
```

## `erased-cast-argument`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.61">0.0.61</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22erased-cast-argument%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1735" target="_blank">View source</a>
</small>


**What it does**

Checks for a basedpython `cast` / `cast?` whose target type carries type
arguments that are erased at runtime.

**Why is this bad?**

A checked cast validates its value with `isinstance`, which can only test
a class — a builtin container erases its type arguments, so `list[int]`
is checkable only as `list`. The cast still narrows the static type to
`list[int]`, but nothing verifies the `int` claim at runtime, which is
exactly the assumption a checked cast exists to rule out.

This only fires where the claim really is assumed. A *user* generic
carries `__orig_class__`, so `A[int]` is checked in full. A value typed by
a *reified* type parameter carries the answer in a runtime cell, so
casting `list[T]` to `list[int]` compares `T == int` exactly. A *protocol*
is checked structurally against the value's reified annotations — data
members against class annotations, method members against
parameter/return annotations. Only a protocol member whose specialized
type has no runtime spelling (a callable attribute) leaves the cast with
no runtime residue, so the whole cast — not just its arguments — is left
unchecked.

**Example**


```by
from typing import Protocol
from collections.abc import Callable

def f(x: object):
    a = x cast list[int]   # warning: only `list` is checked
    b = x cast list        # ok — no argument claimed

class A[T]:
    init(self, t: T)

def g(x: object):
    a = x cast A[int]      # ok — checked in full via `__orig_class__`

def r[T](data: list[T]):
    a = data cast list[int]  # ok — the reified `T` cell decides it

class HasCb[T](Protocol):
    cb: Callable[[T], T]

def h(x: object):
    a = x cast HasCb[int]  # warning: a callable member has no runtime check
```

## `erased-type-check`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.3">0.0.1-alpha.3</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22erased-type-check%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2095" target="_blank">View source</a>
</small>


**What it does**

Checks for parametric type tests (`x is P[int]`) against a protocol,
which has no sound runtime residue.

**Why is this bad?**

A parametric `is` test is answered from static types wherever possible
(Rust-style). When it cannot be — the value's type is dynamic or a mixed
union — the last resort is a runtime probe that unwinds the value's
`__orig_class__` and its class's generic bases across the mro. A protocol
has nothing to unwind: an instance's `__orig_class__` names its concrete
class, never the protocol, and a structural `isinstance` check sees no
type arguments (and raises outright unless the protocol is
`@runtime_checkable`). So the test can never confirm the specialization.

**Example**


```by
from typing import Protocol
class P[T](Protocol):
    def get(self) -> T: ...

def f(x):
    return x is P[int]  # error: a protocol records no specialization
```

Reify the type parameter (so the test compares the reified cell), or test
against a concrete class that fixes the arguments (a user generic, or a
subclass whose `__orig_bases__` records the specialization):

```by
def f[T](x: T):
    return x is list[int]  # ok — compares the reified `T`

class A[T]: ...
def g(x):
    return x is A[int]     # ok — unwinds `x`'s mro
```

## `escape-character-in-forward-annotation`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22escape-character-in-forward-annotation%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fstring_annotation.rs#L40" target="_blank">View source</a>
</small>


**What it does**


Checks for forward annotations that contain escape characters.

**Why is this bad?**


Static analysis tools like ty can't analyze type annotations that contain escape characters.

**Example**


```python
def foo() -> "intt\b": ...  # error
```

## `escaping-local`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22escaping-local%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1211" target="_blank">View source</a>
</small>


**What it does**

Checks for a basedpython `local` parameter whose value escapes the call it
is bound in — returned to the caller, stored on a parameter-rooted object,
assigned to a `global` / `nonlocal` binding, or passed on to a parameter
that is not itself a borrow.

A callable type may declare its own parameters `local` too
(`(local int) -> None`), which puts the same constraint on the trailing
lambda block filling it: the block's implicit `it` is borrowed from the
call.

**Why is this bad?**

A `local` parameter is borrowed only for the duration of the call. Letting
its value outlive the call defeats the borrow: the caller may release the
underlying resource, leaving a dangling reference behind.

**Example**


```by
_saved: object

def f(local fn: () -> None):
    global _saved
    _saved = fn  # error: `fn` is local and cannot escape the call
```

## `escaping-loop-variable`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22escaping-loop-variable%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1700" target="_blank">View source</a>
</small>


**What it does**

Checks for a trailing-lambda block inside a loop that captures a loop
variable while its callee's callback parameter is **not** a borrow
(`local` / `once`).

**Why is this bad?**

A trailing-lambda block lowers to a closure that captures the loop variable
by reference. If the callee is a borrow (`local` / `once`), it runs the
block synchronously — the variable still holds this iteration's value. But
a non-borrow callee may store the block and call it after the loop has
advanced, at which point every deferred call sees the loop variable's final
value — the classic late-binding trap.

This is the type-aware complement to ruff's syntactic `B023`, which cannot
resolve the callee's marker. An opaque callee (not a resolvable function or
bound method) is left alone.

**Example**


```by
def defer(fn: () -> None):  # not a borrow — may keep `fn`
    _saved.append(fn)

for x in [1, 2, 3]:
    defer:
        print(x)  # error: captures loop variable `x`
```

## `experimental-syntax`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.50">0.0.50</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22experimental-syntax%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L233" target="_blank">View source</a>
</small>


**What it does**


Checks for experimental syntax that is not part of the Python typing specification.

**Why is this bad?**


Experimental syntax is specific to ty. It may be rejected by other type checkers and may never be
standardized, or be subject to breaking changes.

**Examples**


```toml
[environment]
python-version = "3.14"
```

```python
class A: ...


class B: ...


def f(value: A & B) -> None: ...  # error: [experimental-syntax]
def g(value: ~A) -> None: ...  # error: [experimental-syntax]
```

## `final-on-non-method`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.20">0.0.20</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22final-on-non-method%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2201" target="_blank">View source</a>
</small>


**What it does**


Checks for `@final` decorators applied to non-method functions.

**Why is this bad?**


The `@final` decorator is only meaningful on methods and classes.
Applying it to a module-level function or a nested function has no
effect and is likely a mistake.

**Example**


```python
from typing import final


# @final is not allowed on non-method functions
@final  # error
def my_function() -> int:
    return 0
```

## `final-on-variable`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.40">0.0.40</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22final-on-variable%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2210" target="_blank">View source</a>
</small>


**What it does**

Checks for the basedpython `final` modifier applied to a bare variable
assignment outside of a class body, e.g. `final a = 1`.

**Why is this bad?**

`final` is a class/method modifier. On a bare assignment it lowers to a
plain assignment and makes the variable no more final than before, so it
is almost certainly a mistake. A final variable is declared with `let`,
which lowers to `Final`.

`final override` is a legitimate assignment marker and is not flagged.

**Example**


```by
# Error: `final` on a variable has no effect
final a = 1

# Correct: `let` declares a final variable
let a = 1
```

## `final-without-value`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.15">0.0.15</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22final-without-value%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2239" target="_blank">View source</a>
</small>


**What it does**


Checks for `Final` symbols that are declared without a value and are never
assigned a value in their scope.

**Why is this bad?**


A `Final` symbol must be initialized with a value at the time of declaration
or in a subsequent assignment. At module or function scope, the assignment must
occur in the same scope. In a class body, the assignment may occur in `__init__`.
Protocol members are declarations of an interface and do not require a value.

**Examples**


```python
from typing import Final

# `Final` symbol without a value
MY_CONSTANT: Final[int]  # error

# OK: `Final` symbol with a value
INITIALIZED_CONSTANT: Final[int] = 1
```

## `ignore-comment-unknown-rule`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22ignore-comment-unknown-rule%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Fsuppression.rs#L46" target="_blank">View source</a>
</small>


**What it does**


Checks for `ty: ignore[code]` or `type: ignore[ty:code]` comments where `code` isn't a known lint rule.

**Why is this bad?**


A `ty: ignore[code]` or a `type: ignore[ty:code]` directive with a `code` that doesn't match
any known rule will not suppress any type errors, and is probably a mistake.

**Examples**


```py
# error
a = 20 / 1  # ty: ignore[division-by-zer]
```

Use instead:

```py
a = 20 / 0  # ty: ignore[division-by-zero]
```

## `implicit-concatenated-string-type-annotation`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22implicit-concatenated-string-type-annotation%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fstring_annotation.rs#L22" target="_blank">View source</a>
</small>


**What it does**


Checks for implicit concatenated strings in type annotation positions.

**Why is this bad?**


Static analysis tools like ty can't analyze type annotations that use implicit concatenated strings.

**Examples**


<!-- fmt:off -->

```python
from typing import Literal

def test() -> "Literal[" "5" "]":  # error
    return 5
```

<!-- fmt:on -->

Use instead:

```python
from typing import Literal


def test() -> "Literal[5]":
    return 5
```

## `implicit-object-repr`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.68">0.0.68</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22implicit-object-repr%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2733" target="_blank">View source</a>
</small>


**What it does**

Checks for a value rendered as text when its class says nothing about
how it should look.

**Why is this bad?**

A class that defines nothing the site can use falls back to the
interpreter's own default, which prints the class name and the address
the object happens to sit at — `<__main__.A object at 0x102bcc6a0>`.
That is never what the message was meant to say, and the address makes
the output differ on every run.

Which dunders count depends on what the site asks for, because the
fallbacks run one way only: `object.__str__` calls `__repr__`, and
`object.__format__` calls `str`, but nothing falls back to `__str__`.

- `repr(x)`, `ascii(x)`, `f"{x!r}"` — only `__repr__`
- `str(x)`, `print(x)`, `f"{x!s}"` — `__str__` or `__repr__`
- `format(x)`, `f"{x}"` — `__format__`, `__str__` or `__repr__`

Only a class written in source is judged. A stub leaves these dunders
out whether or not the runtime class has them — `int` declares none of
the three and still prints as a number — so a class that comes from a
stub, or that inherits from one, is not reported. The exception is a
stub named in `analysis.implicit-object-repr-report-types`, which
defaults to `types.FunctionType` and `builtins.type`: printing a bare
function or class object is the same mistake, and neither stub is
hiding a rendering.

**Options**

- `analysis.implicit-object-repr-exempt-types`
- `analysis.implicit-object-repr-report-types`

**Examples**

```python
class Point:
    def __init__(self, x: int):
        self.x = x

print(Point(1))    # warning: prints `<__main__.Point object at 0x...>`
f"at {Point(1)}"   # warning

class Spoken:
    def __str__(self) -> str:
        return "Spoken()"

print(Spoken())    # ok
repr(Spoken())     # warning: `__str__` is not what `repr` asks for

class Labelled:
    def __repr__(self) -> str:
        return "Labelled()"

print(Labelled())  # ok — `str` falls back to `__repr__`
repr(Labelled())   # ok

def helper() -> None: ...

print(helper)      # warning: prints `<function helper at 0x...>`
print(Labelled)    # warning: prints `<class '__main__.Labelled'>`
```

## `inconsistent-mro`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22inconsistent-mro%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L415" target="_blank">View source</a>
</small>


**What it does**


Checks for classes with an inconsistent [method resolution order] (MRO).

**Why is this bad?**


Classes with an inconsistent MRO will raise a `TypeError` at runtime.

**Examples**


```python
class A: ...


class B(A): ...


# TypeError: Cannot create a consistent method resolution order
class C(A, B): ...  # error
```

[method resolution order]: https://docs.python.org/3/glossary.html#term-method-resolution-order

## `index-out-of-bounds`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22index-out-of-bounds%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L424" target="_blank">View source</a>
</small>


**What it does**


Checks for attempts to use an out of bounds index to get an item from
a container.

**Why is this bad?**


Using an out of bounds index will raise an `IndexError` at runtime.

**Examples**


```python
t = (0, 1, 2)
# IndexError: tuple index out of range
t[3]  # error
```

## `ineffective-final`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.33">0.0.1-alpha.33</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22ineffective-final%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2192" target="_blank">View source</a>
</small>


**What it does**


Checks for calls to `final()` that type checkers cannot interpret.

**Why is this bad?**


The `final()` function is designed to be used as a decorator. When called directly
as a function (e.g., `final(type(...))`), type checkers will not understand the
application of `final` and will not prevent subclassing.

**Example**


```python
from typing import final

# Incorrect: type checkers will not prevent subclassing
MyClass = final(type("MyClass", (), {}))  # error


# Correct: use `final` as a decorator
@final
class MyClass: ...
```

## `instance-layout-conflict`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.12">0.0.1-alpha.12</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22instance-layout-conflict%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L369" target="_blank">View source</a>
</small>


**What it does**


Checks for classes definitions which will fail at runtime due to
"instance memory layout conflicts".

This error is usually caused by attempting to combine multiple classes
that define non-empty `__slots__` in a class's [Method Resolution Order][method-resolution-order]
(MRO), or by attempting to combine multiple builtin classes in a class's
MRO.

**Why is this bad?**


Inheriting from bases with conflicting instance memory layouts
will lead to a `TypeError` at runtime.

An instance memory layout conflict occurs when CPython cannot determine
the memory layout instances of a class should have, because the instance
memory layout of one of its bases conflicts with the instance memory layout
of one or more of its other bases.

For example, if a Python class defines non-empty `__slots__`, this will
impact the memory layout of instances of that class. Multiple inheritance
from more than one different class defining non-empty `__slots__` is not
allowed:

```python
class A:
    __slots__ = ("a", "b")


class B:
    __slots__ = ("a", "b")  # Even if the values are the same


# TypeError: multiple bases have instance lay-out conflict
class C(A, B): ...  # error
```

An instance layout conflict can also be caused by attempting to use
multiple inheritance with two builtin classes, due to the way that these
classes are implemented in a CPython C extension:

```python
# TypeError: multiple bases have instance lay-out conflict
class A(int, float): ...  # error
```

Note that pure-Python classes with no `__slots__`, or pure-Python classes
with empty `__slots__`, are always compatible:

```python
class A: ...


class B:
    __slots__ = ()


class C:
    __slots__ = ("a", "b")


# fine
class D(A, B, C): ...
```

**Known problems**


Classes that have "dynamic" definitions of `__slots__` (definitions do not consist
of string literals, or tuples of string literals) are not currently considered disjoint
bases by ty.

Additionally, this check is not exhaustive: many C extensions (including several in
the standard library) define classes that use extended memory layouts and thus cannot
coexist in a single MRO. Since it is currently not possible to represent this fact in
stub files, having a full knowledge of these classes is also impossible. When it comes
to classes that do not define `__slots__` at the Python level, therefore, ty, currently
only hard-codes a number of cases where it knows that a class will produce instances with
an atypical memory layout.

**Further reading**


- [CPython documentation: `__slots__`](https://docs.python.org/3/reference/datamodel.html#slots)
- [CPython documentation: Method Resolution Order](https://docs.python.org/3/glossary.html#term-method-resolution-order)

[method-resolution-order]: https://docs.python.org/3/glossary.html#term-method-resolution-order

## `invalid-argument-type`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-argument-type%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L461" target="_blank">View source</a>
</small>


**What it does**


Detects call arguments whose type is not assignable to the corresponding typed parameter.

**Why is this bad?**


Passing an argument of a type the function (or callable object) does not accept violates
the expectations of the function author and may cause unexpected runtime errors within the
body of the function.

**Examples**


```python
def func(x: int): ...


func("foo")  # error: [invalid-argument-type]
```

## `invalid-assignment`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-assignment%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L506" target="_blank">View source</a>
</small>


**What it does**


Checks for assignments where the type of the value
is not [assignable to] the type of the assignee.

**Why is this bad?**


Such assignments break the rules of the type system and
weaken a type checker's ability to accurately reason about your code.

**Examples**


```python
a: int = ""  # error
```

[assignable to]: https://typing.python.org/en/latest/spec/glossary.html#term-assignable

## `invalid-attribute-access`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-attribute-access%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2432" target="_blank">View source</a>
</small>


**What it does**


Checks for assignments to class variables from instances
and assignments to instance-only attributes from their class.

An "instance-only" variable is one which is only ever assigned to or declared
when accessed via `self` in an instance method.

**Why is this bad?**


Incorrect assignments break the rules of the type system and
weaken a type checker's ability to accurately reason about your code.

**Examples**


```python
from typing import ClassVar


class C:
    instance_var: int
    class_var: ClassVar[int] = 1

    def __init__(self):
        # instance variable declared in the class body
        self.instance_var = 42

        # instance-only variable not declared in the class body
        self.instance_only_var: int = 42


C.class_var = 3  # okay

C.instance_var = 56  # okay
C().instance_var = 72  # okay

C().instance_only_var = 100  # okay

# Cannot assign to class variable from instance
C().class_var = 3  # error

# Cannot assign to instance-only variable from class
C.instance_only_var = 56  # error
```

## `invalid-attribute-override`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.33">0.0.33</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-attribute-override%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2513" target="_blank">View source</a>
</small>


**What it does**


Detects attribute overrides that change whether an inherited attribute
is a class variable or an instance variable.

This rule currently only covers class-variable and instance-variable
category changes.

**Why is this bad?**


Pure class variables and instance variables have different access and
assignment behavior. Overriding one with the other violates the
[Liskov Substitution Principle][liskov-substitution-principle] ("LSP"), because code that is valid for
the superclass may no longer be valid for the subclass.

**Example**


```python
from typing import ClassVar


class Base:
    instance_attr: int
    class_attr: ClassVar[int]


class Sub(Base):
    instance_attr: ClassVar[int]  # error: [invalid-attribute-override]
    class_attr: int  # error: [invalid-attribute-override]
```

[liskov-substitution-principle]: https://en.wikipedia.org/wiki/Liskov_substitution_principle

## `invalid-await`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.19">0.0.1-alpha.19</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-await%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L515" target="_blank">View source</a>
</small>


**What it does**


Checks for `await` being used with types that are not [Awaitable][awaitable-abc].

**Why is this bad?**


Such expressions will lead to `TypeError` being raised at runtime.

**Examples**


```python
import asyncio


class InvalidAwait:
    def __await__(self) -> int:
        return 5


async def main() -> None:
    await InvalidAwait()  # error: [invalid-await]
    await 42  # error: [invalid-await]


asyncio.run(main())
```

[awaitable-abc]: https://docs.python.org/3/library/collections.abc.html#collections.abc.Awaitable

## `invalid-base`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-base%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L524" target="_blank">View source</a>
</small>


**What it does**


Checks for class definitions that have bases which are not instances of `type`.

**Why is this bad?**


Class definitions with bases like this will lead to `TypeError` being raised at runtime.

**Examples**


```python
class A(42): ...  # error: [invalid-base]
```

## `invalid-context-manager`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-context-manager%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L551" target="_blank">View source</a>
</small>


**What it does**


Checks for expressions used in `with` statements
that do not implement the context manager protocol.

**Why is this bad?**


Such a statement will raise `TypeError` at runtime.

**Examples**


```python
# TypeError: 'int' object does not support the context manager protocol
with 1:  # error
    print(2)
```

## `invalid-conversion`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.39">0.0.1-alpha.39</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-conversion%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1131" target="_blank">View source</a>
</small>


**What it does**

Checks that the basedpython conversion dunders have the shape their
lowered call needs: `__from__` and `__of__` are classmethods on the target
taking one value and returning it, and `__into__` is a plain instance
method on the source taking nothing.

**Why is this bad?**

A conversion site lowers to `Target.__from__(value)` or `value.__into__()`.
A `__from__` that is not a classmethod would bind the value to its first
parameter, and an overloaded `__into__` would have nothing to dispatch on —
so a malformed dunder converts nothing, silently, wherever it was meant to.

**Example**


```by
class Fahrenheit:
    def __from__(cls, value: Celsius) -> Self:  # error: not a classmethod
        ...
```

## `invalid-dataclass`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.12">0.0.12</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-dataclass%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L359" target="_blank">View source</a>
</small>


**What it does**


Checks for invalid applications of the `@dataclass` decorator.

**Why is this bad?**


Applying `@dataclass` with incompatible arguments raises an exception while creating the
class:

- `order=True` with `eq=False`
- `weakref_slot=True` with `slots=False`

Applying `@dataclass` to a class that inherits from `NamedTuple`, `TypedDict`,
`Enum`, or `Protocol` is also invalid:

- `NamedTuple` and `TypedDict` classes will raise an exception at runtime when
    instantiating the class.
- `Enum` classes with `@dataclass` are [explicitly not supported].
- `Protocol` classes define interfaces and cannot be instantiated.

**Examples**


```python
from dataclasses import dataclass
from typing import NamedTuple


@dataclass(order=True, eq=False)  # error: [invalid-dataclass]
class Ordered: ...


@dataclass
class Foo(NamedTuple):  # error: [invalid-dataclass]
    x: int
```

See: <https://docs.python.org/3/library/dataclasses.html#dataclasses.dataclass>

[explicitly not supported]: https://docs.python.org/3/howto/enum.html#dataclass-support

## `invalid-dataclass-override`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.13">0.0.13</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-dataclass-override%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L350" target="_blank">View source</a>
</small>


**What it does**


Checks for dataclass definitions that have both `frozen=True` and a custom `__setattr__` or
`__delattr__` method defined.

**Why is this bad?**


Frozen dataclasses synthesize `__setattr__` and `__delattr__` methods which raise a
`FrozenInstanceError` to emulate immutability.

Overriding either of these methods raises a runtime error.

**Examples**


```python
from dataclasses import dataclass


@dataclass(frozen=True)
class A:
    def __setattr__(self, name: str, value: object) -> None: ...  # error
```

## `invalid-declaration`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-declaration%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L560" target="_blank">View source</a>
</small>


**What it does**


Checks for declarations where the inferred type of an existing symbol
is not [assignable to] its post-hoc declared type.

**Why is this bad?**


Such declarations break the rules of the type system and
weaken a type checker's ability to accurately reason about your code.

**Examples**


```python
a = 1
a: str  # error
```

[assignable to]: https://typing.python.org/en/latest/spec/glossary.html#term-assignable

## `invalid-enum-member-annotation`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.20">0.0.20</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-enum-member-annotation%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L578" target="_blank">View source</a>
</small>


**What it does**


Checks for enum members that have explicit type annotations.

**Why is this bad?**


The [typing spec] states that type checkers should infer a literal type
for all enum members. An explicit type annotation on an enum member is
misleading because the annotated type will be incorrect — the actual
runtime type is the enum class itself, not the annotated type.

In CPython's `enum` module, annotated assignments with values are still
treated as members at runtime, but the annotation will confuse readers of the code.

**Examples**


```python
from enum import Enum


class Pet(Enum):
    CAT = 1  # OK
    # enum members should not be annotated
    DOG: int = 2  # error
```

Use instead:

```python
from enum import Enum


class Pet(Enum):
    CAT = 1
    DOG = 2
```

**References**


- [Typing spec: Enum members](https://typing.python.org/en/latest/spec/enums.html#enum-members)

[typing spec]: https://typing.python.org/en/latest/spec/enums.html#enum-members

## `invalid-exception-caught`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-exception-caught%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L569" target="_blank">View source</a>
</small>


**What it does**


Checks for exception handlers that catch non-exception classes.

**Why is this bad?**


Catching classes that do not inherit from `BaseException` will raise a `TypeError` at runtime.

**Example**


```python
import random


def might_raise() -> float:
    return 1 / random.choice([0, 1, 2, 3, 4, 5])


try:
    might_raise()
except 1:  # error
    ...
```

Use instead:

```python
import random


def might_raise() -> float:
    return 1 / random.choice([0, 1, 2, 3, 4, 5])


try:
    might_raise()
except ZeroDivisionError:
    ...
```

**References**


- [Python documentation: except clause](https://docs.python.org/3/reference/compound_stmts.html#except-clause)
- [Python documentation: Built-in Exceptions](https://docs.python.org/3/library/exceptions.html#built-in-exceptions)

**Ruff rule**


This rule corresponds to Ruff's [`except-with-non-exception-classes` (`B030`)](https://docs.astral.sh/ruff/rules/except-with-non-exception-classes)

## `invalid-explicit-override`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.28">0.0.1-alpha.28</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-explicit-override%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2266" target="_blank">View source</a>
</small>


**What it does**


Checks for methods that are decorated with `@override` but do not override any method in a superclass.

**Why is this bad?**


Decorating a method with `@override` declares to the type checker that the intention is that it should
override a method from a superclass.

**Example**


```toml
[environment]
python-version = "3.12"
```

```python
from typing import override


class A:
    @override
    def foo(self): ...  # error


class B(A):
    @override
    def ffooo(self): ...  # error


class C:
    @override
    def __repr__(self): ...  # fine: overrides `object.__repr__`


class D(A):
    @override
    def foo(self): ...  # fine: overrides `A.foo`
```

## `invalid-extension`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.3">0.0.1-alpha.3</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-extension%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1021" target="_blank">View source</a>
</small>


**What it does**

Checks for invalid basedpython `extension` declarations: an extended
name that does not resolve to a class, a bracket type parameter the
extended type does not declare, or a member that would add stored state.

**Why is this bad?**

An extension adds behaviour to an existing type. Its name must reference
a class declaration, its bracket parameters reuse (and constrain) that
class's own type parameters by name, and its members are resolved at
transpile time with nowhere to store new fields on already-constructed
instances.

**Example**


```by
extension list[T: int]:  # error: `list` declares no type parameter `T`
    def total(self) -> int:
        return sum(self)
```

## `invalid-field-lookup`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.4">0.0.1-alpha.4</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-field-lookup%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1188" target="_blank">View source</a>
</small>


**What it does**

Checks django ORM lookups, `create()` keywords, and field-name string
arguments (`order_by`, `only`, …) against the model's fields.

**Why is this bad?**

The queryset API accepts `**kwargs`, so a mistyped field name or an
operand of the wrong type for a lookup is silently accepted by the
stubs and only fails at runtime.

**Example**


```py
Author.objects.filter(nam="x")            # error: no field `name`
Author.objects.filter(name__startswith=1) # error: lookup wants `str`
```

## `invalid-fixture-type`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.36">0.0.1-alpha.36</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-fixture-type%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1422" target="_blank">View source</a>
</small>


**What it does**

Checks that a pytest test or fixture parameter's type annotation is
compatible with the type provided by the fixture of the same name.

**Why is this bad?**

pytest fills the parameter by name from the fixture registry. If the
annotation drifts from the fixture's real type, the test body is
checked against a type the parameter never actually has, hiding real
errors.

**Example**


```py
import pytest

@pytest.fixture
def user() -> str:
    return "alice"

def test_user(user: int) -> None:  # error: fixture provides `str`
    ...
```

## `invalid-format-spec`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.68">0.0.68</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-format-spec%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2691" target="_blank">View source</a>
</small>


**What it does**

Checks the format spec in an f-string replacement field against the
`__format__` that will read it.

**Why is this bad?**

`f"{value:spec}"` calls `type(value).__format__(value, "spec")`, so a
spec the type does not accept is a `TypeError` or `ValueError` at
runtime, on a line that looks like nothing but text.

Two things are checked. The spec has to be an argument `__format__`
accepts, which for a class that defines none of its own means the empty
spec, because that is all `object.__format__` can do. And when the
`__format__` reached is one of the four that read the [format
specification mini-language] — `str`, `int`, `float`, `complex` — the
spec has to be one those rules allow: `str` has no sign, `int` has no
precision, and neither has the other's presentation types.

A type with a `__format__` of its own outside that set is checked only
as a call. `datetime` reads the same string as strftime codes, and
nothing about the mini-language applies to it.

**Examples**

```python
class Point:
    pass

f"{Point():>10}"  # error: `Point` only has `object.__format__`
f"{'name':d}"     # error: `d` is not a presentation type for `str`
f"{1:.2}"         # error: an integer has no precision
f"{1:.2f}"        # ok — `f` formats the integer as a float
f"{'name':>10}"   # ok
```

[format specification mini-language]: https://docs.python.org/3/library/string.html#format-specification-mini-language

## `invalid-frozen-dataclass-subclass`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.35">0.0.1-alpha.35</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-frozen-dataclass-subclass%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2532" target="_blank">View source</a>
</small>


**What it does**


Checks for dataclasses with invalid frozen inheritance:

- A frozen dataclass cannot inherit from a non-frozen dataclass.
- A non-frozen dataclass cannot inherit from a frozen dataclass.

**Why is this bad?**


Python raises a `TypeError` at runtime when either of these inheritance
patterns occurs.

**Example**


```python
from dataclasses import dataclass


@dataclass
class Base:
    x: int


@dataclass(frozen=True)
class Child(Base):  # error
    y: int


@dataclass(frozen=True)
class FrozenBase:
    x: int


@dataclass
class NonFrozenChild(FrozenBase):  # error
    y: int
```

## `invalid-generic-class`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-generic-class%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L596" target="_blank">View source</a>
</small>


**What it does**


Checks for the creation of invalid generic classes

**Why is this bad?**


There are several requirements that you must follow when defining a generic class.
Many of these result in `TypeError` being raised at runtime if they are violated.

**Examples**


```toml
[environment]
python-version = "3.12"
```

```python
from typing_extensions import Generic, TypeVar

T = TypeVar("T")
U = TypeVar("U", default=int)
V = TypeVar("V", covariant=True)


# class uses both PEP-695 syntax and legacy syntax
class C[U](Generic[T]): ...  # error


# type parameter with default comes before type parameter without default
class D(Generic[U, T]): ...  # error


# covariant type parameter used in a position that requires contravariance
class E(Generic[V]):  # error
    def set(self, value: V) -> None: ...
```

**References**


- [Typing spec: Generics](https://typing.python.org/en/latest/spec/generics.html#introduction)

## `invalid-generic-enum`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.12">0.0.12</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-generic-enum%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L587" target="_blank">View source</a>
</small>


**What it does**


Checks for enum classes that are also generic.

**Why is this bad?**


Enum classes cannot be generic. Python does not support generic enums:
attempting to create one will either result in an immediate `TypeError`
at runtime, or will create a class that cannot be specialized in the way
that a normal generic class can.

**Examples**


```toml
[environment]
python-version = "3.12"
```

```python
from enum import Enum
from typing import Generic, TypeVar

T = TypeVar("T")


# enum class cannot be generic (class creation fails with `TypeError`)
class E[T](Enum):  # error
    A = 1


# enum class cannot be generic (class creation fails with `TypeError`)
class F(Enum, Generic[T]):  # error
    A = 1


# enum class cannot be generic -- the class creation does not immediately fail...
class G(Generic[T], Enum):  # error
    A = 1


# ...but this raises `KeyError`:
x: G[int]
```

**References**


- [Python documentation: Enum](https://docs.python.org/3/library/enum.html)

## `invalid-ignore-comment`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-ignore-comment%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Fsuppression.rs#L55" target="_blank">View source</a>
</small>


**What it does**


Checks for `type: ignore` and `ty: ignore` comments that are syntactically incorrect.

**Why is this bad?**


A syntactically incorrect ignore comment is probably a mistake and is useless.

**Examples**


```py
# error
a = 20 / 1  # type: ignoree
```

Use instead:

```py
a = 20 / 0  # type: ignore
```

## `invalid-implementation`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.5">0.0.1-alpha.5</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-implementation%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1099" target="_blank">View source</a>
</small>


**What it does**

Checks for invalid basedpython `implementation` declarations: an interface
that is neither an abstract class nor a protocol, an implemented name that
does not resolve to a class, a type that already satisfies the interface,
a member that corresponds to nothing on the interface, or a second
implementation of the same pair in one module.

**Why is this bad?**

An implementation states that an existing type satisfies an existing
interface, and lowers to a witness class deriving that interface. An
interface with stored state has nothing for the witness to hold; a type
that already conforms would never be converted, so the block would be dead
code; and a member matching nothing on the interface promises nothing —
an `extension` adds inherent members instead.

**Example**


```by
class Concrete:
    x: int

implementation Concrete for B:  # error: not an abstract class or protocol
    override def f(self): ...
```

## `invalid-key`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.17">0.0.1-alpha.17</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-key%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L434" target="_blank">View source</a>
</small>


**What it does**


Checks for subscript accesses with invalid keys and `TypedDict` construction with an
unknown key.

**Why is this bad?**


Subscripting with an invalid key will raise a `KeyError` at runtime.

Creating a `TypedDict` with an unknown key is likely a mistake; if the `TypedDict` is
`closed=true` it also violates the expectations of the type.

**Examples**


```python
from typing import TypedDict
from typing_extensions import NotRequired


class Person(TypedDict):
    name: NotRequired[str]
    age: NotRequired[int]


alice = Person(name="Alice", age=30)
# KeyError: 'height'
alice["height"]  # error

# error
bob: Person = {"nickname": "Bob", "age": 30}  # typo!

# error
carol = Person(name="Carol", aeg=25)  # typo!
```

## `invalid-legacy-positional-parameter`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.15">0.0.15</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-legacy-positional-parameter%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2550" target="_blank">View source</a>
</small>


**What it does**


Checks for parameters that appear to be attempting to use the legacy convention
to specify that a parameter is positional-only, but do so incorrectly.

The "legacy convention" for specifying positional-only parameters was
specified in [PEP 484][pep-484]. It states that parameters with names starting with
`__` should be considered positional-only by type checkers. [PEP 570][pep-570], introduced
in Python 3.8, added dedicated syntax for specifying positional-only parameters,
rendering the legacy convention obsolete. However, some codebases may still
use the legacy convention for compatibility with older Python versions.

**Why is this bad?**


In most cases, a type checker will not consider a parameter to be positional-only
if it comes after a positional-or-keyword parameter, even if its name starts with
`__`. This may be unexpected to the author of the code.

**Example**


```python
# `__y` is not considered positional-only
def f(x, __y):  # error
    pass
```

Use instead:

```python
def f(__x, __y):  # If you need compatibility with Python <=3.7
    pass
```

or:

```python
def f(x, y, /):  # Python 3.8+ syntax
    pass
```

**References**


- [Typing spec: positional-only parameters (legacy syntax)](https://typing.python.org/en/latest/spec/historical.html#pos-only-double-underscore)
- [Python glossary: parameters](https://docs.python.org/3/glossary.html#term-parameter)

[pep-484]: https://peps.python.org/pep-0484/#positional-only-arguments
[pep-570]: https://peps.python.org/pep-0570/

## `invalid-legacy-type-variable`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-legacy-type-variable%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L614" target="_blank">View source</a>
</small>


**What it does**


Checks for the creation of invalid legacy `TypeVar`s

**Why is this bad?**


There are several requirements that you must follow when creating a legacy `TypeVar`.

**Examples**


```python
from typing import TypeVar

T = TypeVar("T")  # okay
T = TypeVar("T")  # error: "Cannot redefine `T` as a type variable"


# TypeVar must be immediately assigned to a variable
# error
def f(t: TypeVar("U")): ...  # ty: ignore[invalid-type-form]
```

**References**


- [Typing spec: Generics](https://typing.python.org/en/latest/spec/generics.html#introduction)

## `invalid-match-pattern`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.18">0.0.18</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-match-pattern%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L731" target="_blank">View source</a>
</small>


**What it does**


Checks for invalid match patterns.

**Why is this bad?**


Invalid match patterns can cause a `TypeError` at runtime. This includes:

- Using a non-type object in a class pattern.
- Providing positional subpatterns when `__match_args__` is missing or has an invalid static type.
- Matching against `collections.abc.Callable` with positional subpatterns.
- Matching against a non-runtime-checkable protocol.
- Matching against a `TypedDict`.

**Examples**


```python
class Point:
    __match_args__ = ("x", "y")


def describe(p: Point) -> None:
    match p:
        # TypeError at runtime: Point() accepts 2 positional sub-patterns (3 given)
        case Point(x, y, z):  # error: [invalid-match-pattern]
            ...
```

```python
NotAClass = 42

match object():
    # TypeError at runtime: called match pattern must be a class
    case NotAClass():  # error: [invalid-match-pattern]
        ...
```

## `invalid-metaclass`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-metaclass%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L659" target="_blank">View source</a>
</small>


**What it does**


Checks for arguments to `metaclass=` that are invalid.

**Why is this bad?**


Python allows arbitrary expressions to be used as the argument to `metaclass=`.
These expressions, however, need to be callable and accept the same arguments
as `type.__new__`.

**Example**


```python
# TypeError: 'int' object is not callable
class B(metaclass=42): ...  # error
```

**References**


- [Python documentation: Metaclasses](https://docs.python.org/3/reference/datamodel.html#metaclasses)

## `invalid-method-override`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.20">0.0.1-alpha.20</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-method-override%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2522" target="_blank">View source</a>
</small>


**What it does**


Detects method overrides that violate the [Liskov Substitution Principle][liskov-substitution-principle] ("LSP").

The LSP states that an instance of a subtype should be substitutable for an instance of its supertype.
Applied to Python, this means:

1. All argument combinations a superclass method accepts
    must also be accepted by an overriding subclass method.
1. The return type of an overriding subclass method must be a subtype
    of the return type of the superclass method.

**Why is this bad?**


Violating the Liskov Substitution Principle will lead to many of ty's assumptions and
inferences being incorrect, which will mean that it will fail to catch many possible
type errors in your code.

**Example**


```python
class Super:
    def method(self, x) -> int:
        return 42


class Sub(Super):
    # Liskov violation: `str` is not a subtype of `int`,
    # but the supertype method promises to return an `int`.
    def method(self, x) -> str:  # error: [invalid-method-override]
        return "foo"


def accepts_super(s: Super) -> int:
    return s.method(x=42)


# The result of this call is a string, but ty will infer it to be an `int`
# due to the violation of the Liskov Substitution Principle.
accepts_super(Sub())


class Sub2(Super):
    # Liskov violation: the superclass method can be called with a `x=`
    # keyword argument, but the subclass method does not accept it.
    def method(self, y) -> int:  # error: [invalid-method-override]
        return 42


# TypeError at runtime: method() got an unexpected keyword argument 'x'
# ty cannot catch this error due to the violation of the Liskov Substitution Principle.
accepts_super(Sub2())
```

**Common issues**


**Why does ty complain about my `__eq__` method?**


`__eq__` and `__ne__` methods in Python are generally expected to accept arbitrary
objects as their second argument, for example:

```python
class A:
    x: int

    def __eq__(self, other: object) -> bool:
        # gracefully handle an object of an unexpected type
        # without raising an exception
        if not isinstance(other, A):
            return False
        return self.x == other.x
```

If `A.__eq__` here were annotated as only accepting `A` instances for its second argument,
it would imply that you wouldn't be able to use `==` between instances of `A` and
instances of unrelated classes without an exception possibly being raised. While some
classes in Python do indeed behave this way, the strongly held convention is that it should
be avoided wherever possible. As part of this check, therefore, ty enforces that `__eq__`
and `__ne__` methods accept `object` as their second argument.

**Why does ty disagree with Ruff about how to write my method?**


Ruff has several rules that will encourage you to rename a parameter, or change its type
signature, if it thinks you're falling into a certain anti-pattern. For example, Ruff's
[ARG002](https://docs.astral.sh/ruff/rules/unused-method-argument/) rule recommends that an
unused parameter should either be removed or renamed to start with `_`. Applying either of
these suggestions can cause ty to start reporting an [`invalid-method-override`](#invalid-method-override) error if
the function in question is a method on a subclass that overrides a method on a superclass,
and the change would cause the subclass method to no longer accept all argument combinations
that the superclass method accepts.

This can usually be resolved by adding [`@typing.override`][override] to your method
definition. Ruff knows that a method decorated with `@typing.override` is intended to
override a method by the same name on a superclass, and avoids reporting rules like ARG002
for such methods; it knows that the changes recommended by ARG002 would violate the Liskov
Substitution Principle.

Correct use of `@override` is enforced by ty's [`invalid-explicit-override`](#invalid-explicit-override) rule.

[liskov-substitution-principle]: https://en.wikipedia.org/wiki/Liskov_substitution_principle
[override]: https://docs.python.org/3/library/typing.html#typing.override

## `invalid-named-tuple`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.19">0.0.1-alpha.19</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-named-tuple%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L397" target="_blank">View source</a>
</small>


**What it does**


Checks for invalidly defined `NamedTuple` classes.

**Why is this bad?**


An invalidly defined `NamedTuple` class may lead to the type checker
drawing incorrect conclusions. It may also lead to `TypeError`s or
`AttributeError`s at runtime.

**Examples**


A class definition cannot combine `NamedTuple` with other base classes
in multiple inheritance; doing so raises a `TypeError` at runtime. The sole
exception to this rule is `Generic[]`, which can be used alongside `NamedTuple`
in a class's bases list.

```pycon
>>> from typing import NamedTuple
>>> class Foo(NamedTuple, object): ...
TypeError: can only inherit from a NamedTuple type and Generic
```

Further, `NamedTuple` field names cannot start with an underscore:

```pycon
>>> from typing import NamedTuple
>>> class Foo(NamedTuple):
...     _bar: int
ValueError: Field names cannot start with an underscore: '_bar'
```

`NamedTuple` classes also have certain synthesized attributes (like `_asdict`, `_make`,
`_replace`, etc.) that cannot be overwritten. Attempting to assign to these attributes
without a type annotation will raise an `AttributeError` at runtime.

```pycon
>>> from typing import NamedTuple
>>> class Foo(NamedTuple):
...     x: int
...     _asdict = 42
AttributeError: Cannot overwrite NamedTuple attribute _asdict
```

## `invalid-named-tuple-override`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.31">0.0.31</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-named-tuple-override%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L406" target="_blank">View source</a>
</small>


**What it does**


Checks for subclass members that override inherited `NamedTuple` fields.

**Why is this bad?**


Reusing an inherited `NamedTuple` field name in a subclass creates a
class where tuple indexing and `repr()` still reflect the original
field, while attribute access follows the subclass member.

**Default level**


This rule is a warning by default because these overrides do not make
the class invalid at runtime.

**Examples**


```python
from typing import NamedTuple


class User(NamedTuple):
    name: str


class Admin(User):
    name = "shadowed"  # error: [invalid-named-tuple-override]


admin = Admin("Alice")
admin.name  # "shadowed"
admin[0]  # "Alice"
```

## `invalid-newtype`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.27">0.0.1-alpha.27</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-newtype%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L641" target="_blank">View source</a>
</small>


**What it does**


Checks for the creation of invalid `NewType`s

**Why is this bad?**


There are several requirements that you must follow when creating a `NewType`.

**Examples**


```python
from typing import NewType


def get_name() -> str:
    return "name"


Foo = NewType("Foo", int)  # okay
# The first argument to `NewType` must be a string literal
Bar = NewType(get_name(), int)  # error
# invalid base for `typing.NewType`
Baz = NewType("Baz", int | str)  # error
```

## `invalid-overload`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-overload%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L668" target="_blank">View source</a>
</small>


**What it does**


Checks for various invalid `@overload` usages.

**Why is this bad?**


The `@overload` decorator is used to define functions and methods that accepts different
combinations of arguments and return different types based on the arguments passed. This is
mainly beneficial for type checkers. But, if the `@overload` usage is invalid, the type
checker may not be able to provide correct type information.

**Examples**


**Single overload**


```py
from typing import overload


@overload
def foo(x: int) -> int: ...  # error
def foo(x: int | None) -> int | None:
    return x
```

**Missing implementation**


```py
from typing import overload


@overload
def foo() -> None: ...  # error
@overload
def foo(x: int) -> int: ...
```

**References**


- [Python documentation: `@overload`](https://docs.python.org/3/library/typing.html#typing.overload)

## `invalid-parameter-default`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-parameter-default%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L686" target="_blank">View source</a>
</small>


**What it does**


Checks for default values that can't be
assigned to the parameter's annotated type.

**Why is this bad?**


This breaks the rules of the type system and
weakens a type checker's ability to accurately reason about your code.

**Examples**


```python
def f(a: int = ""): ...  # error
```

## `invalid-parametrize`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.36">0.0.1-alpha.36</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-parametrize%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1508" target="_blank">View source</a>
</small>


**What it does**

Checks `@pytest.mark.parametrize` argument names against the decorated
function's parameters, and each parameter set's arity against the
number of names.

**Why is this bad?**

pytest raises a collection error when a parametrized name is not a
function parameter, or when a value row's length does not match the
number of names. These fail only when the test is collected.

**Example**


```py
import pytest

@pytest.mark.parametrize("a, b", [(1, 2), (3,)])  # error: row has 1 value, expected 2
def test_add(a: int, b: int) -> None:
    ...
```

## `invalid-paramspec`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-paramspec%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L623" target="_blank">View source</a>
</small>


**What it does**


Checks for the creation of invalid `ParamSpec`s

**Why is this bad?**


There are several requirements that you must follow when creating a `ParamSpec`.

**Examples**


```python
from typing import ParamSpec

P1 = ParamSpec("P1")  # okay
# ParamSpec requires a name
P2 = ParamSpec()  # error
```

**References**


- [Typing spec: ParamSpec](https://typing.python.org/en/latest/spec/generics.html#paramspec)

## `invalid-protocol`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-protocol%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L378" target="_blank">View source</a>
</small>


**What it does**


Checks for protocol classes that will raise `TypeError` at runtime.

**Why is this bad?**


An invalidly defined protocol class may lead to the type checker inferring
unexpected things. It may also lead to `TypeError`s at runtime.

**Examples**


A `Protocol` class cannot inherit from a non-`Protocol` class;
this raises a `TypeError` at runtime:

```pycon
>>> from typing import Protocol
>>> class Foo(int, Protocol): ...
Traceback (most recent call last):
  File "<python-input-1>", line 1, in <module>
    class Foo(int, Protocol): ...
TypeError: Protocols can only inherit from other protocols, got <class 'int'>
```

## `invalid-raise`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-raise%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L695" target="_blank">View source</a>
</small>


Checks for `raise` statements that raise non-exceptions or use invalid
causes for their raised exceptions.

**Why is this bad?**


Only subclasses or instances of `BaseException` can be raised.
For an exception's cause, the same rules apply, except that `None` is also
permitted. Violating these rules results in a `TypeError` at runtime.

**Examples**


```python
def something():
    raise NameError


def cause() -> None:
    pass


def f():
    try:
        something()
    except NameError:
        # error: "Cannot raise object of type `Literal["oops!"]`"
        # error: "Cannot use object of type `def cause()` as an exception cause"
        raise "oops!" from cause


def g():
    # error: "Cannot raise `NotImplemented`"
    # error: "Cannot use object of type `Literal[42]` as an exception cause"
    raise NotImplemented from 42
```

Use instead:

```python
def something():
    raise NameError


def f():
    try:
        something()
    except NameError as e:
        raise RuntimeError("oops!") from e


def g():
    raise NotImplementedError from None
```

**References**


- [Python documentation: The `raise` statement](https://docs.python.org/3/reference/simple_stmts.html#raise)
- [Python documentation: Built-in Exceptions](https://docs.python.org/3/library/exceptions.html#built-in-exceptions)

## `invalid-raises-clause`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.37">0.0.1-alpha.37</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-raises-clause%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1378" target="_blank">View source</a>
</small>


**What it does**

Checks for a basedpython `raises` clause that does not describe a set of
exceptions.

**Why is this bad?**

Only a `BaseException` subclass can be raised, so a clause with no
exception in it can never be satisfied by anything the function does.

**Example**


```by
def f() raises int:  # error: `int` is not an exception
    ...
```

## `invalid-regex`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.36">0.0.1-alpha.36</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-regex%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1452" target="_blank">View source</a>
</small>


**What it does**

Checks a literal regular expression passed to the `re` module: that it
compiles at all, and that every group a match is asked for exists in it.

**Why is this bad?**

The stubs type every `re` pattern as a plain `str`, so a pattern that
`re.compile` rejects, or a `m.group(3)` on a pattern with two groups,
only fails once the line runs.

**Example**


```py
import re

re.compile("(")  # error: missing ), unterminated subpattern at position 0

if m := re.match("(a)(b)", "ab"):
    m.group(3)  # error: No such group: 3
```

## `invalid-reified-type-param`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.62">0.0.62</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-reified-type-param%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2008" target="_blank">View source</a>
</small>


**What it does**

Checks for a basedpython `reified` type parameter declared somewhere
reification cannot happen.

**Why is this bad?**

Reification is a property of a *function*: the specialization step
rebuilds the function's closure so the body sees the type argument as a
runtime value. A class, a type alias and a `type def` have no such step —
their type parameters are erased — so `reified` there promises a runtime
value that never arrives.

**Example**


```by
class C[reified T]:  # error: a class type parameter is never reified
    ...
```

## `invalid-return-type`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-return-type%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L470" target="_blank">View source</a>
</small>


**What it does**


Detects returned values that can't be assigned to the function's annotated return type.

Note that the special case of a function with a non-`None` return type and an empty body
is handled by the separate [`empty-body`](#empty-body) error code.

**Why is this bad?**


Returning an object of a type incompatible with the annotated return type
is unsound, and will lead to ty inferring incorrect types elsewhere.

**Examples**


```python
def func() -> int:
    return "a"  # error: [invalid-return-type]
```

## `invalid-super-argument`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-super-argument%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L704" target="_blank">View source</a>
</small>


**What it does**


Detects `super()` calls where:

- the first argument is not a valid class literal, or
- the second argument is not an instance or subclass of the first argument.

**Why is this bad?**


`super(type, obj)` expects:

- the first argument to be a class,
- and the second argument to satisfy one of the following:
    - `isinstance(obj, type)` is `True`
    - `issubclass(obj, type)` is `True`

Violating this relationship will raise a `TypeError` at runtime.

**Examples**


```python
class A: ...


class B(A): ...


super(A, B())  # it's okay! `A` satisfies `isinstance(B(), A)`

# `A()` is not a class
super(A(), B())  # error

# `A()` does not satisfy `isinstance(A(), B)`
super(B, A())  # error
# `A` does not satisfy `issubclass(A, B)`
super(B, A)  # error
```

**References**


- [Python documentation: super()](https://docs.python.org/3/library/functions.html#super)

## `invalid-syntax-in-forward-annotation`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-syntax-in-forward-annotation%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fstring_annotation.rs#L31" target="_blank">View source</a>
</small>


**What it does**


Checks for string-literal annotations where the string cannot be
parsed as a Python expression.

**Why is this bad?**


Type annotations are expected to be Python expressions that
describe the expected type of a variable, parameter, attribute or
`return` statement.

Type annotations are permitted to be string-literal expressions, in
order to enable forward references to names not yet defined.
However, it must be possible to parse the contents of that string
literal as a normal Python expression.

**Example**


```python
def foo() -> "instance of C":  # error
    return 42


class C: ...
```

Use instead:

```python
def foo() -> "C":
    return C()


class C: ...
```

**References**


- [Typing spec: The meaning of annotations](https://typing.python.org/en/latest/spec/annotations.html#the-meaning-of-annotations)
- [Typing spec: String annotations](https://typing.python.org/en/latest/spec/annotations.html#string-annotations)

## `invalid-total-ordering`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.10">0.0.10</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-total-ordering%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2541" target="_blank">View source</a>
</small>


**What it does**


Checks for classes decorated with `@functools.total_ordering` that don't
define any ordering method (`__lt__`, `__le__`, `__gt__`, or `__ge__`).

**Why is this bad?**


The `@total_ordering` decorator requires the class to define at least one
ordering method. If none is defined, Python raises a `ValueError` at runtime.

**Example**


```python
from functools import total_ordering


# no ordering method defined
@total_ordering  # error
class MyClass:
    def __eq__(self, other: object) -> bool:
        return True
```

Use instead:

```python
from functools import total_ordering


@total_ordering
class MyClass:
    def __eq__(self, other: object) -> bool:
        return True

    def __lt__(self, other: "MyClass") -> bool:
        return True
```

## `invalid-type-alias-type`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.6">0.0.1-alpha.6</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-type-alias-type%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L632" target="_blank">View source</a>
</small>


**What it does**


Checks for the creation of invalid `TypeAliasType`s

**Why is this bad?**


There are several requirements that you must follow when creating a `TypeAliasType`.

**Examples**


```toml
[environment]
python-version = "3.12"
```

```python
from typing import TypeAliasType


def get_name() -> str:
    return "NewAlias"


IntOrStr = TypeAliasType("IntOrStr", int | str)  # okay
# TypeAliasType name must be a string literal
NewAlias = TypeAliasType(get_name(), int)  # error
```

## `invalid-type-arguments`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.29">0.0.1-alpha.29</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-type-arguments%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L888" target="_blank">View source</a>
</small>


**What it does**


Checks for invalid type arguments in explicit type specialization.

**Why is this bad?**


Providing the wrong number of type arguments or type arguments that don't
satisfy the type variable's bounds or constraints will lead to incorrect
type inference and may indicate a misunderstanding of the generic type's
interface.

**Examples**


Using legacy type variables:

```toml
[environment]
python-version = "3.12"
```

```python
from typing import Generic, TypeVar

T1 = TypeVar("T1", int, str)
T2 = TypeVar("T2", bound=int)


class Foo1(Generic[T1]): ...


class Foo2(Generic[T2]): ...


# bytes does not satisfy T1's constraints
Foo1[bytes]  # error
# str does not satisfy T2's bound
Foo2[str]  # error
```

Using PEP 695 type variables:

```python
class Foo[T]: ...


class Bar[T, U]: ...


# too many arguments
Foo[int, str]  # error
# too few arguments
Bar[int]  # error
```

## `invalid-type-checking-constant`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-type-checking-constant%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L713" target="_blank">View source</a>
</small>


**What it does**


Checks for a value other than `False` assigned to the `TYPE_CHECKING` variable, or an
annotation not assignable from `bool`.

**Why is this bad?**


The name `TYPE_CHECKING` is reserved for a flag that can be used to provide conditional
code seen only by the type checker, and not at runtime. Normally this flag is imported from
`typing` or `typing_extensions`, but it can also be defined locally. If defined locally, it
must be assigned the value `False` at runtime; the type checker will consider its value to
be `True`. If annotated, it must be annotated as a type that can accept `bool` values.

**Examples**


```python
TYPE_CHECKING: str  # error
TYPE_CHECKING = ""  # error
```

## `invalid-type-form`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-type-form%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L722" target="_blank">View source</a>
</small>


**What it does**


Checks for expressions that are used as [type expressions]
but cannot validly be interpreted as such.

**Why is this bad?**


Such expressions cannot be understood by ty.
In some cases, they might raise errors at runtime.

**Examples**


```python
from typing import Annotated

# Int literals are not allowed in this context in type expressions
a: list[1]  # error
# `Annotated` expects at least two arguments
b: Annotated[int]  # error
```

[type expressions]: https://typing.python.org/en/latest/spec/annotations.html#type-and-annotation-expressions

## `invalid-type-guard-definition`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.11">0.0.1-alpha.11</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-type-guard-definition%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L740" target="_blank">View source</a>
</small>


**What it does**


Checks for type guard functions without
a first non-self-like non-keyword-only non-variadic parameter.

**Why is this bad?**


Type narrowing functions must accept at least one positional argument
(non-static methods must accept another in addition to `self`/`cls`).

Extra parameters/arguments are allowed but do not affect narrowing.

**Examples**


```toml
[environment]
python-version = "3.13"
```

```python
from typing import TypeIs


# no parameter
def f() -> TypeIs[int]:  # error
    return True


# no positional arguments allowed
def f(*, v: object) -> TypeIs[int]:  # error
    return True


# expected variadic arguments
def f(*args: object) -> TypeIs[int]:  # error
    return True


class C:
    # only positional argument is `self`
    def f(self) -> TypeIs[int]:  # error
        return True
```

## `invalid-type-variable-bound`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.15">0.0.15</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-type-variable-bound%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L770" target="_blank">View source</a>
</small>


**What it does**


Checks for [type variables][type variable] whose bounds reference type variables.

**Why is this bad?**


The bound of a type variable must be a concrete type.

**Examples**


```toml
[environment]
python-version = "3.12"
```

```python
from typing import TypeVar

# error: [invalid-type-variable-bound]
RecursiveT = TypeVar("RecursiveT", bound=list["RecursiveT"])
U = TypeVar("U")
# error: [invalid-type-variable-bound]
BoundT = TypeVar("BoundT", bound=U)


def f[T: list[T]](): ...  # error: [invalid-type-variable-bound]
def g[U, T: U](): ...  # error: [invalid-type-variable-bound]
```

[type variable]: https://docs.python.org/3/library/typing.html#typing.TypeVar

## `invalid-type-variable-constraints`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-type-variable-constraints%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L761" target="_blank">View source</a>
</small>


**What it does**


Checks for constrained [type variables] with only one constraint,
or that those constraints reference type variables.

**Why is this bad?**


A constrained type variable must have at least two constraints.

**Examples**


```toml
[environment]
python-version = "3.12"
```

```python
from typing import TypeVar

I = TypeVar("I", bound=int)
# constraint references `I`
S = TypeVar("S", list[I], int)  # error


# a constrained type variable needs at least two constraints
def f[T: (int,)](): ...  # error
```

Use instead:

```python
from typing import TypeVar

U = TypeVar("U", str, int)  # valid constrained TypeVar

# or

T = TypeVar("T", bound=str)  # valid bound TypeVar

V = TypeVar("V", list[int], int)  # valid constrained Type
```

[type variables]: https://docs.python.org/3/library/typing.html#typing.TypeVar

## `invalid-type-variable-default`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.16">0.0.16</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-type-variable-default%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L779" target="_blank">View source</a>
</small>


**What it does**


Checks for [type variables] whose default type is not compatible with
the type variable's bound or constraints.

**Why is this bad?**


If a type variable has a bound, the default must be assignable to that
bound (see: [bound rules]). If a type variable has constraints, the default
must be one of the constraints (see: [constraint rules]).

**Examples**


```toml
[environment]
python-version = "3.13"
```

```python
from typing import TypeVar

T = TypeVar("T", bound=str, default=int)  # error: [invalid-type-variable-default]
U = TypeVar("U", int, str, default=bytes)  # error: [invalid-type-variable-default]
```

[bound rules]: https://typing.python.org/en/latest/spec/generics.html#bound-rules
[constraint rules]: https://typing.python.org/en/latest/spec/generics.html#constraint-rules
[type variables]: https://docs.python.org/3/library/typing.html#typing.TypeVar

## `invalid-typed-dict-field`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.28">0.0.28</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-typed-dict-field%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2495" target="_blank">View source</a>
</small>


**What it does**


Detects invalid `TypedDict` field declarations.

**Why is this bad?**


`TypedDict` subclasses cannot redefine inherited fields incompatibly. Doing so breaks the
subtype guarantees that `TypedDict` inheritance is meant to preserve.

**Example**


```python
from typing import TypedDict


class Base(TypedDict):
    x: int


class Child(Base):
    x: str  # error: [invalid-typed-dict-field]
```

## `invalid-typed-dict-header`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.14">0.0.14</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-typed-dict-header%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2504" target="_blank">View source</a>
</small>


**What it does**


Detects errors in `TypedDict` class headers, such as unexpected arguments
or invalid base classes.

**Why is this bad?**


The typing spec states that `TypedDict`s are not permitted to have
custom metaclasses. Using `**` unpacking in a `TypedDict` header
is also prohibited by ty, as it means that ty cannot statically determine
whether keys in the `TypedDict` are intended to be required or optional.

**Example**


```python
from typing import TypedDict


class Meta(type): ...


class Foo(TypedDict, metaclass=Meta):  # error: [invalid-typed-dict-header]
    ...


def f(options: dict[str, object]):
    class Bar(TypedDict, **options):  # error: [invalid-typed-dict-header]
        ...
```

## `invalid-typed-dict-statement`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.9">0.0.9</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-typed-dict-statement%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2486" target="_blank">View source</a>
</small>


**What it does**


Detects statements other than annotated declarations in `TypedDict` class bodies.

**Why is this bad?**


`TypedDict` class bodies aren't allowed to contain any other types of statements. For
example, method definitions and field values aren't allowed. None of these will be
available on "instances of the `TypedDict`" at runtime (as `dict` is the runtime class of
all "`TypedDict` instances").

**Example**


```python
from typing import TypedDict


class Foo(TypedDict):
    def bar(self):  # error: [invalid-typed-dict-statement]
        pass
```

## `invalid-variance-declaration`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.62">0.0.62</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-variance-declaration%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2033" target="_blank">View source</a>
</small>


**What it does**

Checks for a basedpython variance keyword (`in`, `out`, `in out`) that
the program does not honour: one on a type parameter that never
specializes, and one on a type alias that names a variance its expansion
does not have.

**Why is this bad?**

Variance says how two *specializations* relate. A function's type
parameter is solved afresh at each call and never specializes, and a
`type def` is erased before anything could observe one — so the keyword
decides nothing in either position. Reported rather than dropped, because
a keyword nothing checks reads as a promise that something does.

A generic class and a generic type alias both specialize and both keep
the keyword. An alias has no variance of its own — it relates exactly as
the type it expands to does — so its keyword is a claim about that
expansion, and a claim the expansion contradicts is reported too. A
class's own keyword is checked against its members by
[`invalid-generic-class`](#invalid-generic-class).

**Example**


```by
def f[out T](t: T) -> None: ...  # error: a function's `T` has no variance

type Alias[out T] = list[T]      # error: `list` is invariant
```

## `invalid-yield`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.25">0.0.25</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22invalid-yield%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L479" target="_blank">View source</a>
</small>


**What it does**


Detects `yield` and `yield from` expressions where the "yield" or "send" type
is incompatible with the generator function's annotated return type.

**Why is this bad?**


Yielding a value of a type that doesn't match the generator's declared yield type,
or using `yield from` with a sub-iterator whose yield or send type is incompatible,
is a type error that may cause downstream consumers of the generator to receive
values of an unexpected type.

**Examples**


```python
from typing import Iterator


def gen() -> Iterator[int]:
    yield "not an int"  # error: [invalid-yield]
```

## `isinstance-against-protocol`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.14">0.0.14</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22isinstance-against-protocol%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L443" target="_blank">View source</a>
</small>


**What it does**


Reports invalid runtime checks against `Protocol` classes.
This includes explicit calls `isinstance()`/`issubclass()` against
non-runtime-checkable protocols, `issubclass()` calls against protocols
that have non-method members, and implicit `isinstance()` checks against
non-runtime-checkable protocols via pattern matching.

**Why is this bad?**


These calls (implicit or explicit) raise `TypeError` at runtime.

**Examples**


```python
from typing_extensions import Protocol, runtime_checkable


class HasX(Protocol):
    x: int


@runtime_checkable
class HasY(Protocol):
    y: int


def f(arg: object, arg2: type):
    # not runtime-checkable
    isinstance(arg, HasX)  # error: [isinstance-against-protocol]
    # not runtime-checkable
    issubclass(arg2, HasX)  # error: [isinstance-against-protocol]


def g(arg: object):
    match arg:
        # not runtime-checkable
        case HasX():  # error: [isinstance-against-protocol]
            pass


def h(arg2: type):
    isinstance(arg2, HasY)  # fine (runtime-checkable)

    # `HasY` is runtime-checkable, but has non-method members,
    # so it still can't be used in `issubclass` checks)
    issubclass(arg2, HasY)  # error: [isinstance-against-protocol]
```

**References**


- [Typing documentation: `@runtime_checkable`](https://docs.python.org/3/library/typing.html#typing.runtime_checkable)

## `isinstance-against-typed-dict`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.15">0.0.15</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22isinstance-against-typed-dict%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L452" target="_blank">View source</a>
</small>


**What it does**


Reports runtime checks against `TypedDict` classes.
This includes explicit calls to `isinstance()`/`issubclass()` and implicit
checks performed by `match` class patterns.

**Why is this bad?**


Using a `TypedDict` class in these contexts raises `TypeError` at runtime.

**Examples**


```python
from typing_extensions import TypedDict


class Movie(TypedDict):
    name: str
    director: str


def f(arg: object, arg2: type):
    isinstance(arg, Movie)  # error: [isinstance-against-typed-dict]
    issubclass(arg2, Movie)  # error: [isinstance-against-typed-dict]


def g(arg: object):
    match arg:
        case Movie():  # error: [isinstance-against-typed-dict]
            pass
```

**References**


- [Typing specification: `TypedDict`](https://typing.python.org/en/latest/spec/typeddict.html)

## `iteration-over-character`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.36">0.0.1-alpha.36</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22iteration-over-character%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2140" target="_blank">View source</a>
</small>


**What it does**

Checks for iteration over a basedpython `Character` (a single-character string).

**Why is this bad?**

A `Character` always has a length of exactly 1, so iterating over it yields a
single element — the `Character` itself. Code that does this almost always
meant to iterate over the enclosing string instead, or believed the value
was a longer string.

**Example**


```by
def f(s: str):
    first = s[0]
    for c in first:  # warning: iterating over a `Character`
        ...
```

## `mismatched-type-name`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.30">0.0.30</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22mismatched-type-name%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L650" target="_blank">View source</a>
</small>


**What it does**


Checks for functional typing definitions whose declared name does not match
the variable they are assigned to.

**Why is this bad?**


Constructors like `TypeVar`, `ParamSpec`, `NewType`, `NamedTuple`,
`TypedDict`, and `TypeAliasType` all take a name argument that is
normally expected to match the assigned variable. A mismatch is usually a
typo and makes later diagnostics harder to understand.

**Default level**


This rule is a warning by default because ty can usually recover and
continue understanding the resulting type.

**Examples**


```python
from typing import NewType, ParamSpec, TypeVar
from typing_extensions import TypedDict

T = TypeVar("U")  # error: [mismatched-type-name]
P = ParamSpec("Q")  # error: [mismatched-type-name]
UserId = NewType("Id", int)  # error: [mismatched-type-name]
Movie = TypedDict("Film", {"title": str})  # error: [mismatched-type-name]
```

## `missing-argument`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22missing-argument%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L797" target="_blank">View source</a>
</small>


**What it does**


Checks for missing required arguments in a call.

**Why is this bad?**


Failing to provide a required argument will raise a `TypeError` at runtime.

**Examples**


```python
def func(x: int): ...


# TypeError: func() missing 1 required positional argument: 'x'
func()  # error
```

## `missing-context-argument`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.61">0.0.61</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22missing-context-argument%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1920" target="_blank">View source</a>
</small>


**What it does**

Checks for calls that leave a basedpython `context` parameter unfilled:
no explicit argument matches it and no `context` declaration in scope
has a type assignable to it.

**Why is this bad?**

A `context` parameter has no default — the call raises `TypeError` at
runtime unless something supplies the argument.

**Examples**

```python
def f(a: int, context b: str): ...

f(1)                  # error: no context value in scope
context s = "hello"
f(1)                  # ok — `s` is passed implicitly
```

## `missing-framework-stubs`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.4">0.0.1-alpha.4</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22missing-framework-stubs%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1048" target="_blank">View source</a>
</small>


**What it does**

Checks for imports of frameworks that ship no inline type annotations
when their external PEP 561 stubs package is not installed.

**Why is this bad?**

Without the stubs package the framework's types resolve from its untyped
runtime source, so most framework-aware checking silently degrades to
`Unknown`. Installing the stubs package restores precise types.

**Example**


```py
from django.db import models  # warning: install `django-stubs` for precise types
```

## `missing-override-decorator`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'ignore'."><code>ignore</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.41">0.0.41</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22missing-override-decorator%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2275" target="_blank">View source</a>
</small>


**What it does**


Checks for methods that override a method or attribute in a superclass but are not decorated with `@override`.

This rule is disabled by default. Enable it to opt in to strict `@override` enforcement for a project.

**Exemptions**


Overriding `__init__`, `__new__`, `__init_subclass__`, or `__post_init__` does not require
`@override`, even if the method is explicitly declared by a superclass.

**Why is this bad?**


Without an `@override` annotation, refactors can silently change whether a method is an override.
Requiring `@override` on every override lets ty report when an intended override stops overriding
anything, and when a method unexpectedly starts overriding a superclass member.

**Example**


```toml
[environment]
python-version = "3.12"
```

```python
from typing import override


class Parent:
    def method(self) -> int:
        return 1


class Child(Parent):
    # when the rule is enabled
    def method(self) -> int:  # error
        return 2


class ExplicitChild(Parent):
    @override
    def method(self) -> int:  # fine
        return 2
```

## `missing-type-argument`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'ignore'."><code>ignore</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.45">0.0.45</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22missing-type-argument%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L806" target="_blank">View source</a>
</small>


**What it does**


Checks for generic types used without type parameters in type expressions.

**Why is this bad?**


Using a generic type without specifying its type parameters results in the
type parameters being implicitly filled with `Unknown`, reducing the
precision of type checking. Explicit type parameters make the intended types
clear and enable the type checker to catch more errors.

**Examples**


```python
import re


def handle(m: re.Match) -> str:  # error: [missing-type-argument]
    return m.string


# Use explicit type parameters instead:
def handle(m: re.Match[str]) -> str:
    return m.string
```

## `missing-typed-dict-key`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.20">0.0.1-alpha.20</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22missing-typed-dict-key%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2477" target="_blank">View source</a>
</small>


**What it does**


Detects missing required keys in `TypedDict` constructor calls.

**Why is this bad?**


`TypedDict` requires all non-optional keys to be provided during construction.
Missing items can lead to a `KeyError` at runtime.

**Example**


```python
from typing import TypedDict


class Person(TypedDict):
    name: str
    age: int


# missing required key 'age'
alice: Person = {"name": "Alice"}  # error

alice["age"]  # KeyError
```

## `narrowing-guard-as-value`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22narrowing-guard-as-value%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1615" target="_blank">View source</a>
</small>


**What it does**

Checks for a call to a basedpython assertion guard whose result is used as a value.

**Why is this bad?**

An assertion guard narrows once it *returns*, so it is written as a statement:
`check(x)`. Its value is `None` — it raises when the assertion doesn't hold — so
testing that value (`if check(x):`) or binding it (`ok = check(x)`) never gets the
narrowing, and the test is always false.

**Example**


```by
def check(x: int | None) -> asserts x:
    if x is None:
        raise ValueError

def f(a: int | None):
    if check(a):  # error: the guard narrows as a statement, not as a test
        ...
```

## `no-matching-overload`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22no-matching-overload%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L870" target="_blank">View source</a>
</small>


**What it does**


Checks for calls to an overloaded function that do not match any of the overloads.

**Why is this bad?**


Failing to provide the correct arguments to one of the overloads will raise a `TypeError`
at runtime.

**Examples**


```python
from typing import overload


@overload
def func(x: int): ...
@overload
def func(x: bool): ...
def func(x: int | bool): ...


func("string")  # error: [no-matching-overload]
```

## `non-callable-init-subclass`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.30">0.0.30</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22non-callable-init-subclass%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L605" target="_blank">View source</a>
</small>


**What it does**


Checks for class definitions that will fail due to non-callable `__init_subclass__`
methods.

**Why is this bad?**


If a class defines a non-callable `__init_subclass__` method/attribute, any attempt
to subclass that class will raise a `TypeError` at runtime.

**Examples**


```python
class Super:
    __init_subclass__ = None


class Sub(Super): ...  # error: [non-callable-init-subclass]
```

**References**


- [Python data model: Customizing class creation](https://docs.python.org/3/reference/datamodel.html#customizing-class-creation)

## `non-exhaustive-statement-expression`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.39">0.0.1-alpha.39</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22non-exhaustive-statement-expression%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1329" target="_blank">View source</a>
</small>


**What it does**

Checks for a basedpython statement expression that can complete without
producing a value.

**Why is this bad?**

A statement expression stands where a value is expected. If some path
through it reaches the end of the statement without evaluating a tail
expression or a `break <value>`, there is no value to stand in.

**Examples**

```by
def f(x: int | str) -> int:
    # error: no value when `x` is a `str`
    return match x:
        case int():
            1
```

## `non-overlapping-cast`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.61">0.0.61</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22non-overlapping-cast%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1790" target="_blank">View source</a>
</small>


**What it does**

Checks for a `cast` whose value type is disjoint from the target type,
so no value could ever belong to both.

**Why is this bad?**

A cast between non-overlapping types can never succeed: a checked `cast`
always raises at runtime, and a safe `cast?` always yields `None`. The
cast is almost certainly a mistake.

**Examples**

```by
def f(a: object):
    a cast int       # ok — `object` overlaps `int`
    "" cast int      # warning: `str` and `int` are disjoint
```

## `non-overlapping-type-test`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.62">0.0.62</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22non-overlapping-type-test%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1813" target="_blank">View source</a>
</small>


**What it does**

Checks for an `is` / `is not` type test whose tested value is disjoint
from the type it is tested against, so no value could ever belong to both.

**Why is this bad?**

The test has a constant answer: `is` is always `False` and `is not` is
always `True`. Either the branch it guards is dead code, or the wrong type
was named.

The value's *narrowed* type is what is tested, which is often sharper
than its declaration: a constructor call is a `final Shape`, a value whose
runtime class is exactly `Shape`'s.

**Examples**

```by
class Shape

def f(x: None):
    if x is int:          # warning: `None` and `int` are disjoint
        ...
    s = Shape()
    if s is str:          # warning: `final Shape` and `str` are disjoint
        ...

def g(o: object, shape: Shape):
    if o is int:          # ok — `object` overlaps `int`
        ...
    if shape is str:      # ok — a `Shape` subclass could inherit `str`
        ...
```

## `not-iterable`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22not-iterable%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L897" target="_blank">View source</a>
</small>


**What it does**


Checks for objects that are not iterable but are used in a context that requires them to be.

**Why is this bad?**


Iterating over an object that is not iterable will raise a `TypeError` at runtime.

**Examples**


```python
# TypeError: 'int' object is not iterable
for i in 34:  # error
    pass
```

## `not-subscriptable`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22not-subscriptable%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L879" target="_blank">View source</a>
</small>


**What it does**


Checks for subscripting objects that do not support subscripting.

**Why is this bad?**


Subscripting an object that does not support it will raise a `TypeError` at runtime.

**Examples**


```python
# TypeError: 'int' object is not subscriptable
4[1]  # error
```

## `once-called-twice`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22once-called-twice%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1535" target="_blank">View source</a>
</small>


**What it does**

Checks for a basedpython `once` callback parameter that a function body may
call more than once — two unconditional calls, or a call inside a loop.

**Why is this bad?**

A `once` callback must be called exactly once. Calling it again — or in a
loop that may run more than once — breaks that contract.

**Example**


```by
def f(once done: () -> None):
    for _ in range(3):
        done()  # error: `done` may be called more than once
```

## `once-not-called`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22once-not-called%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1400" target="_blank">View source</a>
</small>


**What it does**

Checks for a basedpython `once` callback parameter that the function body
never calls.

**Why is this bad?**

A `once` callback must be called exactly once. A body that never mentions
it has forgotten to call it — a common completion-handler bug.

**Example**


```by
def f(once done: () -> None):
    do_work()  # error: `done` is never called
```

## `optional-object-conversion`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.61">0.0.61</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22optional-object-conversion%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1851" target="_blank">View source</a>
</small>


**What it does**

Checks for an optional value passed where `object` is expected, silently
discarding a layer of optionality.

**Why is this bad?**

`object` swallows both the present value and `None`, so consuming an
optional as `object` loses the information that the value could be
absent. It is almost always unintended: either the value should be
unwrapped first with `!`, or the widening should be made explicit with
`cast object`.

Assigning an optional to a declared `object` variable is *not* flagged —
the target narrows back to the optional type, so nothing is lost there.
The diagnostic surfaces at each use of the value as `object` instead.

**Examples**

```by
def sink(o: object): ...

def f(x: int?):
    sink(x)             # warning: `int | None` widened to `object`
    sink(x!)            # ok — unwrapped first
    sink(x cast object) # ok — explicit
```

## `overlapping-condition`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.62">0.0.62</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22overlapping-condition%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2559" target="_blank">View source</a>
</small>


**What it does**

Checks for a truthiness test whose selected branch holds a value that is
always there alongside one that is only sometimes there.

**Why is this bad?**

A condition collapses a value to a single bit, so two members that answer
the test the same way become indistinguishable inside the branch. The case
that bites is a sentinel meeting the falsy corner of a value that normally
belongs to the other branch — `None` meeting `""` in `if not name:`.

Only the branch the condition *selects* is analyzed: `if a` looks at the
truthy members, `if not a` at the falsy ones. A boolean operator is one
condition per operand rather than one over its value, since each operand's
truthiness is tested on its own.

Two members that are *each* only partly in the branch are not reported:
`if x:` over a `str | bytes` conflates nothing the union did not already
conflate. Neither are members of one class, or of two classes where one
derives the other, which a condition was never going to tell apart:
`Literal[1] | Literal[2]` and `list[A] | list[B]` are each one kind of
value.

**Examples**

```python
def f(a: bool | None):
    if a:      # ok — only `True` is truthy
        ...
    if not a:  # warning: `False` and `None` are both falsy
        ...

def g(name: str | None):
    if not name:  # warning: `""` and `None` are both falsy
        ...
    if name is None:  # ok — the members are told apart
        ...
```

**Options**

- `analysis.overlapping-condition-exempt-types`
- `analysis.overlapping-condition-assume-truthy-instances`

## `override-of-final-method`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.29">0.0.1-alpha.29</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22override-of-final-method%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2174" target="_blank">View source</a>
</small>


**What it does**


Checks for methods on subclasses that override superclass methods decorated with `@final`.

**Why is this bad?**


Decorating a method with `@final` declares to the type checker that it should not be
overridden on any subclass.

**Example**


```python
from typing import final


class A:
    @final
    def foo(self): ...


class B(A):
    def foo(self): ...  # error
```

## `override-of-final-variable`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.16">0.0.16</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22override-of-final-variable%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2183" target="_blank">View source</a>
</small>


**What it does**


Checks for class variables on subclasses that override a superclass variable
that has been declared as `Final`.

**Why is this bad?**


Declaring a variable as `Final` indicates to the type checker that it should not be
overridden on any subclass.

**Example**


```python
from typing import Final


class A:
    X: Final[int] = 1


class B(A):
    X = 2  # error
```

## `override-raise`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'ignore'."><code>ignore</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.38">0.0.1-alpha.38</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22override-raise%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1269" target="_blank">View source</a>
</small>


**What it does**

Checks for a method that can raise an exception the method it overrides
cannot.

**Why is this bad?**

A call is checked against the type it can see. When a base-class method
cannot raise, its callers are told nothing escapes — but a subclass
substituted for the base can still raise from that call, and no caller
on the base type has any reason to handle it.

This is off by default: it is a strictness option, and honouring it means
a base method's exception set bounds every override of it.

**Example**


```by
class A:
    def foo(self): ...

class B(A):
    override def foo(self):
        raise TypeError  # error: `A.foo` cannot raise

def get() -> A:
    return B()

def main():
    get().foo()  # nothing here says this can raise
```

## `parameter-already-assigned`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22parameter-already-assigned%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L915" target="_blank">View source</a>
</small>


**What it does**


Checks for calls which provide more than one argument for a single parameter.

**Why is this bad?**


Providing multiple values for a single parameter will raise a `TypeError` at runtime.

**Examples**


```python
def f(x: int) -> int:
    return x


f(1, x=2)  # error
```

## `positional-only-parameter-as-kwarg`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.22">0.0.1-alpha.22</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22positional-only-parameter-as-kwarg%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2360" target="_blank">View source</a>
</small>


**What it does**


Checks for keyword arguments in calls that match positional-only parameters of the callable.

**Why is this bad?**


Providing a positional-only parameter as a keyword argument will raise `TypeError` at runtime.

**Example**


```python
def f(x: int, /) -> int:
    return x


f(x=1)  # error
```

## `possibly-missing-attribute`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'ignore'."><code>ignore</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.22">0.0.1-alpha.22</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22possibly-missing-attribute%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L924" target="_blank">View source</a>
</small>


**What it does**


Checks for possibly missing attributes.

**Why is this bad?**


Attempting to access a missing attribute will raise an `AttributeError` at runtime.

**Rule status**


This rule is currently disabled by default because of the number of
false positives it can produce.

**Examples**


```python
class A:
    if __name__ == "__main__":
        c = 0


# AttributeError: type object 'A' has no attribute 'c'
A.c  # error
```

## `possibly-missing-implicit-call`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.22">0.0.1-alpha.22</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22possibly-missing-implicit-call%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L260" target="_blank">View source</a>
</small>


**What it does**


Checks for implicit calls to possibly missing methods.

**Why is this bad?**


Expressions such as `x[y]` and `x * y` call methods
under the hood (`__getitem__` and `__mul__` respectively).
Calling a missing method will raise an `AttributeError` at runtime.

**Examples**


```python
import datetime


class A:
    if datetime.date.today().weekday() != 6:

        def __getitem__(self, v): ...


# TypeError: 'A' object is not subscriptable
A()[0]  # error
```

## `possibly-missing-import`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'ignore'."><code>ignore</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.22">0.0.1-alpha.22</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22possibly-missing-import%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L942" target="_blank">View source</a>
</small>


**What it does**


Checks for imports of symbols that may be missing.

**Why is this bad?**


Importing a missing module or name will raise a `ModuleNotFoundError`
or `ImportError` at runtime.

**Rule status**


This rule is currently disabled by default because of the number of
false positives it can produce.

**Examples**


`module.py`:

```python
import datetime

if datetime.date.today().weekday() != 6:
    a = 1
```

`main.py`:

```python
# ImportError: cannot import name 'a' from 'module'
from module import a  # error
```

## `possibly-missing-submodule`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.23">0.0.23</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22possibly-missing-submodule%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L933" target="_blank">View source</a>
</small>


**What it does**


Checks for accesses of submodules that might not've been imported.

**Why is this bad?**


When module `a` has a submodule `b`, `import a` isn't generally enough to let you access
`a.b.` You either need to explicitly `import a.b`, or else you need the `__init__.py` file
of `a` to include `from . import b`. Without one of those, `a.b` is an `AttributeError`.

**Examples**


```python
import html

# AttributeError: module 'html' has no attribute 'parser'
html.parser  # error
```

## `possibly-unresolved-reference`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'ignore'."><code>ignore</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22possibly-unresolved-reference%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L951" target="_blank">View source</a>
</small>


**What it does**


Checks for references to names that are possibly not defined.

**Why is this bad?**


Using an undefined variable will raise a `NameError` at runtime.

**Rule status**


This rule is currently disabled by default because of the number of
false positives it can produce.

**Example**


```python
for i in range(int(input())):
    x = i

# NameError: name 'x' is not defined
print(x)  # error
```

## `private-import`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22private-import%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L995" target="_blank">View source</a>
</small>


**What it does**

Checks for imports of a symbol another module declared `private`.

**Why is this bad?**

A `private` declaration is part of its module's implementation, not its
interface. It is renamed with a leading underscore by the lowering, so an
importing module is reaching past a boundary the author drew explicitly,
and the symbol may be renamed or removed without notice.

**Example**


```by
# helpers.by
private type Key = str | int

# main.by
from helpers import Key  # error: `Key` is private to `helpers`
```

## `pydantic-discarded-extra-argument`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.60">0.0.60</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22pydantic-discarded-extra-argument%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2347" target="_blank">View source</a>
</small>


**What it does**


Checks for extra keyword arguments that Pydantic silently discards when a model uses
`extra="ignore"`, either implicitly or explicitly.

**Why is this bad?**


A discarded argument has no effect on the constructed model, but it may indicate a misspelled field
name or an incorrect assumption about the model's schema.

**Example**


```python {data-mdtest="ignore"}
from pydantic import BaseModel


class User(BaseModel):
    name: str
    admin: bool = False


user = User(name="Alice", admni=True)  # error: [pydantic-discarded-extra-argument]
```

If the field name has been misspelled, fix the typo. Otherwise, consider removing the extra argument,
or explicitly configure the model with `extra="allow"`.

## `raw-string-type-annotation`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22raw-string-type-annotation%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fstring_annotation.rs#L13" target="_blank">View source</a>
</small>


**What it does**


Checks for raw-strings in type annotation positions.

**Why is this bad?**


Static analysis tools like ty can't analyze type annotations that use raw-string notation.

**Examples**


```python
def test() -> r"int":  # error
    return 1
```

Use instead:

```python
def test() -> "int":
    return 1
```

## `redundant-boolean-comparison`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.62">0.0.62</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22redundant-boolean-comparison%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2655" target="_blank">View source</a>
</small>


**What it does**

Checks for a comparison of a `bool` against `True` or `False`.

**Why is this bad?**

The operand is already the value the comparison produces, so the
comparison only adds noise. Testing the operand — or its negation —
says the same thing.

An operand that is *not* a `bool` is left alone: `x == True` where
`x: bool | None` really does tell `True` apart from `False` and `None`,
and `x == True` where `x: int` is a value comparison. A chained comparison
(`a == True == b`) is left alone too: it is two comparisons over one
literal, and neither the operand nor its negation replaces the chain.

**Examples**

```python
def f(a: bool):
    if a == True:   # warning: redundant comparison
        ...
    if a is False:  # warning: redundant comparison
        ...
    if a:           # ok
        ...

def g(a: bool | None):
    if a == True:  # ok — tells `True` apart from `False` and `None`
        ...
```

## `redundant-cast`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22redundant-cast%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2441" target="_blank">View source</a>
</small>


**What it does**


Detects redundant `cast` calls where the value already has the target type.

**Why is this bad?**


These casts have no effect and can be removed.

**Example**


```python
from typing import cast


def f() -> int:
    return 10


# Redundant
cast(int, f())  # error
```

## `redundant-condition`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.62">0.0.62</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22redundant-condition%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2607" target="_blank">View source</a>
</small>


**What it does**

Checks for a condition whose outcome is fixed by the tested type, so the
branch is not conditional at all.

**Why is this bad?**

One of the two branches is dead. Either the annotation is wider than the
author believed, or the test is left over from an earlier version of the
code.

Only a value *read* is reported — a name, an attribute, a subscript. A
comparison or a call computes a fresh value, and ty folding that one is the
statically-known-branch machinery doing its job: `elif isinstance(x, B):`
closing an exhaustive chain is deliberate, and so is `while True:`, which
is a literal rather than a read.

A read whose constant outcome comes from the checker's model of the build
environment (`TYPE_CHECKING`, `sys.version_info`, `sys.platform`, `os.name`)
rather than from the program's own types is not reported either — those are
*artificially* constant, and selecting a branch at check time is exactly
what they are for.

**Examples**

```python
import sys
from typing import TYPE_CHECKING, Literal

def f(a: Literal[True], x: int):
    if a:  # warning: always true
        ...
    if x is not None:  # ok — a comparison, not a value read
        ...

while True:  # ok — a literal, not a value read
    ...

if TYPE_CHECKING:  # ok — artificially constant
    ...
if sys.version_info >= (3, 12):  # ok — artificially constant
    ...
```

## `redundant-final-classvar`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.18">0.0.18</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22redundant-final-classvar%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2450" target="_blank">View source</a>
</small>


**What it does**


Checks for redundant combinations of the `ClassVar` and `Final` type qualifiers.

**Why is this bad?**


An attribute that is marked `Final` in a class body is implicitly a class variable.
Marking it as `ClassVar` is therefore redundant.

Note that this diagnostic is not emitted for dataclass fields or protocol members,
where `ClassVar[Final[int]]` has a distinct meaning from `Final[int]`.

**Examples**


```python
from typing import ClassVar, Final


class C:
    # redundant
    x: ClassVar[Final[int]] = 1  # error
    # redundant
    y: Final[ClassVar[int]] = 1  # error
```

## `redundant-return-annotation`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.62">0.0.62</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22redundant-return-annotation%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2801" target="_blank">View source</a>
</small>


**What it does**

Checks for an explicit `-> None` return annotation that leaves the function's
type exactly where deleting it would.

**Why is this bad?**

Once unannotated signatures are recovered, `None` is what a `def` already
means when it says nothing about its return type, so writing it out adds a
word without adding information. Leaving it off is also how the type reads
back: `reveal_type` of such a function shows `def f()`, not `def f() -> None`.

Nothing is reported when neither option below is on — there `-> None` is
load-bearing, since removing it makes the function return `Unknown`.

Where the type would come from instead does not matter, only whether it is
still `None`. A `def` that would return something else keeps its annotation: a
body that always raises returns `Never`, a generator returns a generator, and
an override or an overload implementation returns whatever it inherits.

**Examples**

```python
def f() -> None:  # warning: redundant `-> None`
    print("hi")

def g():  # ok — says the same thing
    print("hi")

def h() -> None:  # ok — the body returns `Never`, not `None`
    raise ValueError

class Base:
    def m(self) -> int | None: ...

class Sub(Base):
    def m(self) -> None:  # ok — without it, `m` would return `int | None`
        print("hi")
```

**Options**

- `analysis.infer-unannotated-signatures`
- `analysis.sound-types`

## `refutable-destructuring`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.62">0.0.62</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22refutable-destructuring%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L497" target="_blank">View source</a>
</small>


**What it does**


Checks for a basedpython destructuring binder whose pattern may not match the
value it destructures, with nothing to handle the failure.

**Why is this bad?**


A destructuring binder — a `let` statement, a `for` target, a `with` item, a
parameter — binds its captures unconditionally. A pattern that does not match
leaves them unbound, which is a `NameError` at the first use.

A `let` statement can handle the failure with an `else` block, but only if the
block diverges: control that falls out of it reaches the same unbound captures.

**Examples**


```by
def f(value: int | str) -> int:
    let int(n) := value  # error: [refutable-destructuring]
    return n

def g(value: int | str) -> int:
    let int(n) := value else:  # error: [refutable-destructuring]
        print("not an int")
    return n  # error: [possibly-unresolved-reference]
```

Use a pattern that matches every value of the type, or an `else` block that
diverges:

```by
def f(value: int | str) -> int:
    let int(n) := value else:
        return 0
    return n
```

## `reified-classmethod`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.3">0.0.1-alpha.3</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22reified-classmethod%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2068" target="_blank">View source</a>
</small>


**What it does**

Checks for classmethods whose type parameters are reified.

**Why is this bad?**

A type parameter referenced in a value position is *reified*: the
specialization step (`f[int]`) becomes a runtime operation that
rebuilds the function's closure with the type arguments. The
classmethod binding hides the underlying function behind an opaque
bound method, so a reified classmethod can be neither specialized
nor called at runtime.

**Example**


```by
class C:
    @classmethod
    def make[T](cls) -> object:
        return T()  # error: `T` is reified, but `make` is a classmethod
```

## `shadowed-type-variable`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.20">0.0.20</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22shadowed-type-variable%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2459" target="_blank">View source</a>
</small>


**What it does**


Checks for type variables in nested generic classes or functions that shadow type variables
from an enclosing scope.

**Why is this bad?**


Shadowing type variables makes the code confusing and is disallowed by the typing spec.

**Examples**


```toml
[environment]
python-version = "3.12"
```

```python
class Outer[T]:
    # `T` is already used by `Outer`
    class Inner[T]: ...  # error

    # `T` is already used by `Outer`
    def method[T](self, x: T) -> T:  # error
        return x
```

**References**


- [Typing spec: Generics](https://typing.python.org/en/latest/spec/generics.html#introduction)

## `static-assert-error`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22static-assert-error%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2423" target="_blank">View source</a>
</small>


**What it does**


Makes sure that the argument of `static_assert` is statically known to be true.

**Why is this bad?**


A `static_assert` call represents an explicit request from the user
for the type checker to emit an error if the argument cannot be verified
to evaluate to `True` in a boolean context.

**Examples**


```python
from ty_extensions import static_assert

# evaluates to `False`
static_assert(1 + 1 == 3)  # error

# does not have a statically known truthiness
static_assert(int(2.0 * 3.0) == 6)  # error
```

## `subclass-of-dataclass-with-order`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.39">0.0.39</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22subclass-of-dataclass-with-order%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2165" target="_blank">View source</a>
</small>


**What it does**


Checks for classes that inherit from a dataclass with `order=True`.

**Why is this bad?**


When a dataclass has `order=True`, comparison methods (`__lt__`, `__le__`, `__gt__`, `__ge__`)
are generated that compare instances as tuples of their fields. These methods raise a
`TypeError` at runtime when comparing instances of different classes in the inheritance
hierarchy, even if one is a subclass of the other.

This violates the [Liskov Substitution Principle][liskov-substitution-principle] because child class instances cannot be
used in all contexts where parent class instances are expected.

**Example**


```python
from dataclasses import dataclass


@dataclass(order=True)
class Parent:
    value: int


class Child(Parent):  # error
    pass


# At runtime, this raises TypeError:
# Child(1) < Parent(2)
```

Consider using [`functools.total_ordering`][total_ordering] instead, which does not have this limitation.

[liskov-substitution-principle]: https://en.wikipedia.org/wiki/Liskov_substitution_principle
[total_ordering]: https://docs.python.org/3/library/functools.html#functools.total_ordering

## `subclass-of-final-class`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22subclass-of-final-class%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L960" target="_blank">View source</a>
</small>


**What it does**


Checks for classes that subclass final classes.

**Why is this bad?**


Decorating a class with `@final` declares to the type checker that it should not be subclassed.

**Example**


```python
from typing import final


@final
class A: ...


class B(A): ...  # error
```

## `subclass-of-sealed-class`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22subclass-of-sealed-class%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L969" target="_blank">View source</a>
</small>


**What it does**

Checks for classes that subclass a basedpython `sealed` class from outside
the workspace in which the sealed class is defined.

**Why is this bad?**

A `sealed` class declares a closed set of subclasses. It may be subclassed
freely from anywhere within its own workspace, but not from a dependency,
so that the set of subclasses is fully known.

**Example**


```by
# in a dependency:
sealed class Shape

# in your workspace:
class Circle(Shape): ...  # error: `Shape` is sealed in another workspace
```

## `super-call-in-named-tuple-method`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.30">0.0.1-alpha.30</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22super-call-in-named-tuple-method%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2320" target="_blank">View source</a>
</small>


**What it does**


Checks for calls to `super()` inside methods of `NamedTuple` classes.

**Why is this bad?**


Using `super()` in a method of a `NamedTuple` class will raise an exception at runtime.

**Examples**


```python
from typing import NamedTuple


class F(NamedTuple):
    x: int

    def method(self):
        # super() is not supported in methods of NamedTuple classes
        super()  # error
```

**References**


- [Python documentation: super()](https://docs.python.org/3/library/functions.html#super)

## `too-many-positional-arguments`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22too-many-positional-arguments%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2302" target="_blank">View source</a>
</small>


**What it does**


Checks for calls that pass more positional arguments than the callable can accept.

**Why is this bad?**


Passing too many positional arguments will raise `TypeError` at runtime.

**Example**


```python
def f(): ...


f("foo")  # error
```

## `trailing-lambda-control-flow`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22trailing-lambda-control-flow%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1558" target="_blank">View source</a>
</small>


**What it does**

Checks for a `return` in a trailing-lambda block whose callback is **not**
`once`.

**Why is this bad?**

A block bound to a `once` callback runs exactly once, like a `with` body,
so its `return` may target the enclosing function. A non-`once` callback
may run any number of times (or not at all), so the block is an ordinary
closure — a `return` would leave the block, not the enclosing scope, which
is almost never what the caller intends.

A `break` / `continue` that would leave the block is already reported as
being outside a loop (the block is a function scope), so this lint covers
only `return`.

**Example**


```by
def each(items: list[int], fn: (int) -> None): ...

def find(items: list[int]) -> int:
    each(items):
        return it  # error: `return` in a non-`once` block leaves the block
    return -1
```

## `trailing-lambda-parameters`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22trailing-lambda-parameters%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1672" target="_blank">View source</a>
</small>


**What it does**

Checks for a trailing-lambda block whose callback takes arguments the
block has no parameter for.

**Why is this bad?**

A block binds one argument, as `it` — plus its callback's implicit
receiver, which the body spells `self` and reads members off
unqualified. A callback that takes more than that, or takes a variadic
parameter, passes arguments the block cannot name, and would be called
with more arguments than it declares.

**Example**


```by
def f(a: (int, str) -> None):
    a(1, "two")

f:  # error: the block binds only `it`, so `"two"` has nowhere to go
    print(it)
```

## `trailing-lambda-return-type`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22trailing-lambda-return-type%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1643" target="_blank">View source</a>
</small>


**What it does**

Checks for a trailing-lambda block whose callback's declared return type
does not accept `None`.

**Why is this bad?**

A trailing-lambda block lowers to a function that returns `None` (in a
`once` block a `return` targets the *enclosing* function, not the block).
A callback whose return type does not accept `None` — `int`, `str`, … —
can never be satisfied by the block. A return type that merely accepts
`None` (`None` itself, `int | None`, `object`) is fine; other non-`None`
return types are not yet supported.

**Example**


```by
def f(a: (int) -> str):  # callback returns `str`
    print(a(1))

f:  # error: the block returns `None`, not `str`
    print(it)
```

## `type-assertion-failure`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22type-assertion-failure%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2284" target="_blank">View source</a>
</small>


**What it does**


Checks for `assert_type()` and `assert_never()` calls where the actual type
is not the same as the asserted type.

**Why is this bad?**


`assert_type()` allows confirming the inferred type of a certain value.

**Example**


```toml
[environment]
python-version = "3.11"
```

```python
from typing import assert_type


def _(x: int):
    assert_type(x, int)  # fine
    # Actual type does not match asserted type
    assert_type(x, str)  # error
```

## `unannotated-model-field`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.4">0.0.1-alpha.4</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unannotated-model-field%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1244" target="_blank">View source</a>
</small>


**What it does**

Checks for a pydantic model field assigned a field specifier
(`Field(...)`) without a type annotation.

**Why is this bad?**

Pydantic requires every field to be annotated. An unannotated
`name = Field(...)` is not collected as a field and raises
`PydanticUserError` when the model class is created.

**Example**


```py
from pydantic import BaseModel, Field

class User(BaseModel):
    name = Field(default="")  # error: needs a type annotation
```

## `unavailable-implicit-super-arguments`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unavailable-implicit-super-arguments%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2311" target="_blank">View source</a>
</small>


**What it does**


Detects invalid `super()` calls where implicit arguments like the enclosing class or first method argument are unavailable.

**Why is this bad?**


When `super()` is used without arguments, Python tries to find two things:
the nearest enclosing class and the first argument of the immediately enclosing function (typically self or cls).
If either of these is missing, the call will fail at runtime with a `RuntimeError`.

**Examples**


```python
# no enclosing class or function found
super()  # error


def func():
    # no enclosing class or first argument exists
    super()  # error


class A:
    # no enclosing function to provide the first argument
    f = super()  # error

    def method(self):
        def nested():
            # first argument does not exist in this nested function
            super()  # error

        # first argument does not exist in this lambda
        lambda: super()  # error

        # argument is not available in generator expression
        (super() for _ in range(10))  # error

        super()  # okay! both enclosing class and first argument are available
```

**References**


- [Python documentation: super()](https://docs.python.org/3/library/functions.html#super)

## `unbound-type-variable`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.20">0.0.20</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unbound-type-variable%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L788" target="_blank">View source</a>
</small>


**What it does**


Checks for type variables that are used in a scope where they are not bound
to any enclosing generic context.

**Why is this bad?**


Using a type variable outside of a scope that binds it has no well-defined meaning.

**Examples**


```python
from typing import TypeVar, Generic

T = TypeVar("T")
S = TypeVar("S")

# unbound type variable in module scope
x: T  # error


class C(Generic[T]):
    # S is not in this class's generic context
    x: list[S] = []  # error
```

**References**


- [Typing spec: Scoping rules for type variables](https://typing.python.org/en/latest/spec/generics.html#scoping-rules-for-type-variables)

## `undeclared-raise`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.37">0.0.1-alpha.37</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22undeclared-raise%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1306" target="_blank">View source</a>
</small>


**What it does**

Checks for a basedpython function that can raise an exception its
`raises` clause does not include.

**Why is this bad?**

A `raises` clause is the function's contract about what can escape a
call to it. Callers rely on that set to decide what they must handle, so
an exception outside it escapes somewhere nobody expects it.

**Example**


```by
def f() raises TypeError:
    raise ValueError  # error: `ValueError` is not declared
```

## `undefined-reveal`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22undefined-reveal%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2329" target="_blank">View source</a>
</small>


**What it does**


Checks for calls to `reveal_type` without importing it.

**Why is this bad?**


Using `reveal_type` without importing it will raise a `NameError` at runtime.

**Examples**


```python
# NameError: name 'reveal_type' is not defined
# error
reveal_type(1)  # revealed: Literal[1]
```

## `unhandled-exception`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.37">0.0.1-alpha.37</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unhandled-exception%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1354" target="_blank">View source</a>
</small>


**What it does**

Checks for an exception that can escape a basedpython `main` function.

**Why is this bad?**

`main` is the program's entry point, so it has no caller to handle what
it raises. An exception escaping it terminates the program with a
traceback rather than an error the program chose to report.

**Example**


```by
def read() raises OSError: ...

def main():
    read()  # error: `OSError` can escape `main`
```

## `unknown-argument`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unknown-argument%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2338" target="_blank">View source</a>
</small>


**What it does**


Checks for keyword arguments in calls that don't match any parameter of the callable.

**Why is this bad?**


Providing an unknown keyword argument will raise `TypeError` at runtime.

**Example**


```python
def f(x: int) -> int:
    return x


f(x=1, y=2)  # error
```

## `unknown-fixture`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'ignore'."><code>ignore</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.36">0.0.1-alpha.36</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unknown-fixture%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1479" target="_blank">View source</a>
</small>


**What it does**

Checks a pytest test or fixture parameter that resolves to no known
fixture.

**Why is this bad?**

pytest fails such a request at collection time with a "fixture not
found" error. A renamed or misspelled fixture leaves a parameter that
no provider satisfies.

**Off by default**

Third-party plugins inject fixtures through `pytest11` entry points,
which this check does not yet discover, so it would false-positive on
any plugin-provided fixture. It ships as an opt-in lint until plugin
discovery lands.

**Example**


```py
def test_thing(no_such_fixture) -> None:  # requests an unknown fixture
    ...
```

## `unresolved-attribute`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unresolved-attribute%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2369" target="_blank">View source</a>
</small>


**What it does**


Checks for unresolved attributes.

**Why is this bad?**


Accessing an unbound attribute will raise an `AttributeError` at runtime.
An unresolved attribute is not guaranteed to exist from the type alone,
so this could also indicate that the object is not of the type that the user expects.

**Examples**


```python
class A: ...


# AttributeError: 'A' object has no attribute 'foo'
A().foo  # error
```

## `unresolved-global`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.15">0.0.1-alpha.15</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unresolved-global%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2468" target="_blank">View source</a>
</small>


**What it does**


Detects variables declared as `global` in an inner scope that have no explicit
bindings or declarations in the global scope.

**Why is this bad?**


Function bodies with `global` statements can run in any order (or not at all), which makes
it hard for static analysis tools to infer the types of globals without
explicit definitions or declarations.

**Example**


**Assigning without a global-scope declaration**


```python
def f():
    # unresolved global
    global x  # error
    x = 42


def g():
    print(x)  # unresolved reference
```

**Use instead**


**Declare the global**


```python
x: int


def f():
    global x
    x = 42


def g():
    print(x)
```

**Initialize the global**


```python
x: int | None = None


def f():
    global x
    x = 42


def g():
    print(x)
```

## `unresolved-import`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unresolved-import%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2378" target="_blank">View source</a>
</small>


**What it does**


Checks for import statements for which the module cannot be resolved.

**Why is this bad?**


Importing a module that cannot be resolved will raise a `ModuleNotFoundError`
at runtime.

**Examples**


```python
# ModuleNotFoundError: No module named 'foo'
import foo  # error
```

## `unresolved-narrowing-guard`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unresolved-narrowing-guard%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1591" target="_blank">View source</a>
</small>


**What it does**

Checks for a basedpython narrowing return annotation whose place doesn't exist.

**Why is this bad?**

`-> asserts x` and `-> x is T` name the place a call narrows. The name is
resolved against the function's parameters, and otherwise against the places
visible where the guard is written. A name that is neither — a typo, or a
parameter that was later renamed — silently narrows nothing at every call site.

**Example**


```by
def check(value: int | None) -> asserts values:  # error: `values` is nothing
    if value is None:
        raise ValueError
```

## `unresolved-reference`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unresolved-reference%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2387" target="_blank">View source</a>
</small>


**What it does**


Checks for references to names that are not defined.

**Why is this bad?**


Using an undefined variable will raise a `NameError` at runtime.

**Example**


```python
# NameError: name 'x' is not defined
print(x)  # error
```

## `unspecialized-reified-generic`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.3">0.0.1-alpha.3</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unspecialized-reified-generic%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L1972" target="_blank">View source</a>
</small>


**What it does**

Checks for calls to a basedpython reified generic function whose
specialization is neither written explicitly nor inferable from the
arguments.

**Why is this bad?**

A function whose type parameter is referenced in a value position is
*reified*: the type parameter behaves like a positional parameter that
is filled by the `[...]` specialization step. A bare call is legal only
when the transpiler can inject that step — every type parameter must
solve, from the arguments or its PEP 696 default, to a type with a
runtime spelling at the call site. Otherwise the parameter has no
value at runtime.

**Example**


```by
def f[T](t: object):
    print(T)

f[int](1)  # ok
f(1)       # error: `T` appears nowhere in the signature

def g[T](t: T):
    print(T)

g(1)       # ok — transpiles to g[int](1)
```

## `unsupported-base`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.7">0.0.1-alpha.7</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unsupported-base%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L533" target="_blank">View source</a>
</small>


**What it does**


Checks for class definitions that have bases which are unsupported by ty.

**Why is this bad?**


If a class has a base that is an instance of a complex type such as a union type,
ty will not be able to resolve the [method resolution order] (MRO) for the class.
This will lead to an inferior understanding of your codebase and unpredictable
type-checking behavior.

**Examples**


```python
import datetime


class A: ...


class B: ...


if datetime.date.today().weekday() != 6:
    C = A
else:
    C = B


class D(C): ...  # error: [unsupported-base]
```

[method resolution order]: https://docs.python.org/3/glossary.html#term-method-resolution-order

## `unsupported-bool-conversion`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unsupported-bool-conversion%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L906" target="_blank">View source</a>
</small>


**What it does**


Checks for bool conversions where the object doesn't correctly implement `__bool__`.

**Why is this bad?**


If an exception is raised when you attempt to evaluate the truthiness of an object,
using the object in a boolean context will fail at runtime.

**Examples**


```python
class NotBoolable:
    __bool__ = None

    def __lt__(self, other: object) -> "NotBoolable":
        return self


b1 = NotBoolable()
b2 = NotBoolable()

# exception raised here
if b1:  # error
    pass

# exception raised here
b1 and b2  # error
# exception raised here
not b1  # error

# A chained comparison converts the result of `b1 < b2` to bool.
# exception raised here
b1 < b2 < b1  # error
```

## `unsupported-dynamic-base`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'ignore'."><code>ignore</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.12">0.0.12</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unsupported-dynamic-base%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L542" target="_blank">View source</a>
</small>


**What it does**


Checks for dynamic class definitions (using `type()`) that have bases
which are unsupported by ty.

This is equivalent to [`unsupported-base`](#unsupported-base) but applies to classes created
via `type()` rather than `class` statements.

**Why is this bad?**


If a dynamically created class has a base that is an unsupported type
such as `type[T]`, ty will not be able to resolve the
[method resolution order] (MRO) for the class. This may lead to an inferior
understanding of your codebase and unpredictable type-checking behavior.

**Default level**


This rule is disabled by default because it will not cause a runtime error,
and may be noisy on codebases that use `type()` in highly dynamic ways.

**Examples**


```python
class Base: ...


def factory(base: type[Base]) -> type:
    # `base` has type `type[Base]`, not `type[Base]` itself
    return type("Dynamic", (base,), {})  # error: [unsupported-dynamic-base]
```

[method resolution order]: https://docs.python.org/3/glossary.html#term-method-resolution-order

## `unsupported-operator`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unsupported-operator%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2396" target="_blank">View source</a>
</small>


**What it does**


Checks for binary expressions, comparisons, and unary expressions where
the operands don't support the operator.

**Why is this bad?**


Attempting to use an unsupported operator will raise a `TypeError` at
runtime.

**Examples**


```python
class A: ...


# TypeError: unsupported operand type(s) for +: 'A' and 'A'
A() + A()  # error
```

## `unused-awaitable`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.21">0.0.21</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unused-awaitable%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2405" target="_blank">View source</a>
</small>


**What it does**


Checks for awaitable objects (such as coroutines) used as expression
statements without being awaited.

**Why is this bad?**


Calling an `async def` function returns a coroutine object. If the
coroutine is never awaited, the body of the async function will never
execute, which is almost always a bug. Python emits a
`RuntimeWarning: coroutine was never awaited` at runtime in this case.

**Examples**


```python
async def fetch_data() -> str:
    return "data"


async def main() -> None:
    # Warning: coroutine is not awaited
    fetch_data()  # error
    await fetch_data()  # OK
```

## `unused-ignore-comment`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unused-ignore-comment%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Fsuppression.rs#L28" target="_blank">View source</a>
</small>


**What it does**


Checks for `ty: ignore` directives that are no longer applicable.

**Why is this bad?**


A `ty: ignore` directive that no longer matches any diagnostic violations is likely
included by mistake, and should be removed to avoid confusion.

**Examples**


```py
# error
a = 20 / 2  # ty: ignore[division-by-zero]
```

Use instead:

```py
a = 20 / 2
```

**Options**


Set [`analysis.respect-type-ignore-comments`](https://docs.astral.sh/ty/reference/configuration/#respect-type-ignore-comments)
to `false` to prevent this rule from reporting unused `type: ignore` comments.

## `unused-type-ignore-comment`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.14">0.0.14</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22unused-type-ignore-comment%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Fsuppression.rs#L37" target="_blank">View source</a>
</small>


**What it does**


Checks for `type: ignore` directives that are no longer applicable.

**Why is this bad?**


A `type: ignore` directive that no longer matches any diagnostic violations is likely
included by mistake, and should be removed to avoid confusion.

**Examples**


```py
# error
a = 20 / 2  # type: ignore
```

Use instead:

```py
a = 20 / 2
```

**Options**


This rule is skipped if [`analysis.respect-type-ignore-comments`](https://docs.astral.sh/ty/reference/configuration/#respect-type-ignore-comments)
to `false`.

## `useless-overload-body`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'warn'."><code>warn</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.22">0.0.1-alpha.22</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22useless-overload-body%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L677" target="_blank">View source</a>
</small>


**What it does**


Checks for various `@overload`-decorated functions that have non-stub bodies.

**Why is this bad?**


Functions decorated with `@overload` are ignored at runtime; they are overridden
by the implementation function that follows the series of overloads. While it is
not illegal to provide a body for an `@overload`-decorated function, it may indicate
a misunderstanding of how the `@overload` decorator works.

**Example**


```toml
[environment]
python-version = "3.11"
```

```py
from typing import overload


@overload
def foo(x: int) -> int:
    # will never be executed
    return x + 1  # error


@overload
def foo(x: str) -> str:
    # will never be executed
    return "Oh no, got a string"  # error


def foo(x: int | str) -> int | str:
    raise Exception("unexpected type encountered")
```

Use instead:

```py
from typing import assert_never, overload


@overload
def foo(x: int) -> int: ...


@overload
def foo(x: str) -> str: ...


def foo(x: int | str) -> int | str:
    if isinstance(x, int):
        return x + 1
    elif isinstance(x, str):
        return "Oh no, got a string"
    else:
        assert_never(x)
```

**References**


- [Python documentation: `@overload`](https://docs.python.org/3/library/typing.html#typing.overload)

## `zero-stepsize-in-slice`

<small>
Default level: <a href="../../rules#rule-levels" title="This lint has a default level of 'error'."><code>error</code></a> ·
Added in <a href="https://github.com/astral-sh/ty/releases/tag/0.0.1-alpha.1">0.0.1-alpha.1</a> ·
<a href="https://github.com/astral-sh/ty/issues?q=sort%3Aupdated-desc%20is%3Aissue%20is%3Aopen%20%22zero-stepsize-in-slice%22" target="_blank">Related issues</a> ·
<a href="https://github.com/astral-sh/ruff/blob/main/crates%2Fty_python_semantic%2Fsrc%2Ftypes%2Fdiagnostic.rs#L2414" target="_blank">View source</a>
</small>


**What it does**


Checks for a step size of zero in slices when the operation is known to fail.

**Why is this bad?**


Python's built-in sequence types raise a `ValueError` when sliced with a step size of zero.

**Known problems**


This check is not exhaustive. It reports zero-step slices for certain built-in sequence
types where the operation is known to fail. A custom `__getitem__` implementation can
accept or reject such a slice, so ty cannot detect every runtime failure.

**Examples**


```python
values = list(range(10))
# ValueError: slice step cannot be zero
values[1:10:0]  # error

tuple_values = (1, 2, 3)
# ValueError: slice step cannot be zero
tuple_values[1:10:0]  # error
```

