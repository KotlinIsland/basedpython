# modifiers and visibility

basedpython promotes commonly-used decorators and `typing` annotations into first-class
keyword modifiers on classes, functions, and assignments. the surface keywords replace
boilerplate decorator/annotation pairs at transpile time:

```by
data class Point:
    x: int
    y: int

    override def __repr__(self) -> str:
        return f"({self.x}, {self.y})"

let ORIGIN = Point(0, 0)
```

transpiles to:

```python
from dataclasses import dataclass
from typing import Final
from typing_extensions import override

@dataclass(slots=True)
class Point:
    x: int
    y: int

    @override
    def __repr__(self) -> str:
        return f"({self.x}, {self.y})"

ORIGIN: Final = Point(0, 0)
```

## class modifiers

| basedpython             | Python output                                       |
| ----------------------- | --------------------------------------------------- |
| `final class Foo`       | `@final` + `class Foo`                              |
| `abstract class Foo`    | `class Foo` (keyword stripped, no decorator)        |
| `open class Foo`        | `class Foo` (keyword stripped, no decorator)        |
| `data class Foo`        | `@dataclass(slots=True)` + `class Foo`              |
| `frozen data class Foo` | `@dataclass(frozen=True, slots=True)` + `class Foo` |
| `protocol Foo`          | `class Foo(Protocol)` (base added)                  |
| `sealed class Foo`      | `class Foo` + `Foo.__sealed_members__ = (...)`      |

`abstract` is a marker for the type checker; it has no runtime decorator.
`open` is the inverse of `final` — a marker that the class is intended to be
subclassed. neither emits a runtime artefact

`sealed` declares a closed subclass hierarchy — see
[sealed classes](sealed-classes.md)

`frozen data class` rejects every attribute write after construction, so its
fields are read-only and the class is inferred [covariant](variance.md) in
their types:

```by
frozen data class D[T]:
    t: T

d: D[object] = D[int](t=1)   # ok — `D` is covariant in `T`
```

a plain `data class` is mutable, so it stays invariant. the same rule applies
to any frozen dataclass-like class: `@dataclass(frozen=True)`, a
`@dataclass_transform(frozen_default=True)` base, a frozen pydantic model, and
individual pydantic fields marked `Field(frozen=True)`

`enum class` is not a modifier but its own declaration form — see
[based enums](enums.md)

## function modifiers

| basedpython      | Python output           |
| ---------------- | ----------------------- |
| `final def m`    | `@final def m`          |
| `abstract def m` | `@abstractmethod def m` |
| `override def m` | `@override def m`       |
| `static def m`   | `@staticmethod def m`   |
| `class def m`    | `@classmethod def m`    |

`override` is sourced from `typing` on 3.12+ and `typing_extensions` below.
`abstract def` with no body is filled in with `: raise NotImplementedError`
instead of the usual `: ...`

`override`, `static` and `class` are *method* modifiers: they say how a class
dispatches one of its members, or that the member replaces one it inherits. a
`def` that no class body owns is not a member of anything, so writing one on it
is an error:

```by
static def helper()   # error: `static` is only a modifier on a method
```

`final`, `abstract` and the visibility keywords read on a function wherever it
is written

## let / var / class-var / newtype

| basedpython                 | Python output                          |
| --------------------------- | -------------------------------------- |
| `let MAX = 100`             | `MAX: Final = 100`                     |
| `let x: int`                | `x: Final[int]`                        |
| `let x`                     | `x: Final`                             |
| `var x = 1`                 | `x = 1`                                |
| `var x: int = 1`            | `x: int = 1`                           |
| `var x: int`                | `x: int`                               |
| `class count = 0`           | `count: ClassVar = 0` (inside a class) |
| `class var count: int`      | `count: ClassVar[int]`                 |
| `class let ORIGIN: P = P()` | `ORIGIN: Final[P] = P()`               |
| `newtype UserId = int`      | `UserId = NewType("UserId", int)`      |

`let` works at module and class scope. inside a class, `class x = ...` is the
class-variable form (distinct from the regular `let x = ...` which is `Final`).
the initializer may be omitted: `let x: int` declares a read-only attribute and
a bare `let x` an uninitialized `Final`, both bound by a single later assignment.
`newtype` introduces a distinct `typing.NewType`-backed type at module scope

a `let` or `var` written inside a block — an `if` body, a loop body, a `try` clause —
belongs to that block and is gone after it. see [block scoping](block-scoping.md)

`class var x: T` is the same class variable with its type *declared* rather than
read off a value, which is the only form a stub can write. `class let x: T = v`
is the read-only one — python spells that `Final`, which in a class body cannot
be reassigned through the class or an instance. a `class let` needs a value:
`__init__` binds an instance, so there is no later place for one to arrive.
`class` is a class-body modifier — at module scope there is no class for the
variable to belong to, and it is an error there

## var

`var` is the mutable counterpart of `let`: it marks the declaration site of a
variable and nothing else. the keyword is stripped at transpile time and the
statement means exactly what the assignment under it means — no `Final`, and an
untyped `var` puts no declared type on the name:

```by
var count = 0
count = 1        # fine; `let count = 0` would reject this

var name: str = ""
name = 1         # error: `str` is declared
```

`var` works at module, class, and function scope, and composes with the
modifier keywords (`private var x = 1`). unlike `let`, it may not be written
bare: `var x` states neither a type nor a value, so there is nothing to declare
and it is rejected — write `var x: T` or `let x`

`var` on an `init(...)` parameter is a different feature — the attribute
shorthand described in [init method](init-method.md)

## assignment modifiers

`override`, `final override`, and `abstract` may also appear on assignments
and annotated assignments. the modifier keyword is stripped at transpile time:

| basedpython            | Python output |
| ---------------------- | ------------- |
| `override x = 1`       | `x = 1`       |
| `final override x = 1` | `x = 1`       |
| `abstract x: T`        | `x: T`        |

these are compile-time-only markers — they constrain how the symbol is
checked but emit no runtime artefact

a bare `final x = 1` (with no `override`) is not an assignment modifier: it would
strip to a plain `x = 1` and declare nothing final. outside a class body ty
rejects it with `final-on-variable` and points you to `let`, which lowers to
`Final`. inside a class body it is a plain attribute, matching `let` there, and
is not flagged

## export / public

basedpython infers `__all__` from explicit visibility keywords:

```by
export def public_api(): ...
public def also_exported(): ...
private def helper(): ...
```

transpiles to:

```python
__all__ = ["public_api", "also_exported"]

def public_api(): ...
def also_exported(): ...
def _helper(): ...
```

`export` and `public` are aliases. each marked symbol is added to a synthesized
`__all__` list at module level. inside a class body they are stripped: a class
member is not a module export

a module-level `private` strips the keyword and gives the symbol a leading
underscore at the definition site *and* every same-module call site. a name that
already has one keeps it — a second would make it a `__name`, which python
name-mangles wherever a class body reads it. it is excluded from `__all__` even
when no `export`/`public` declarations exist, and another module that imports it
is reported by `private-import`

a module-level `private` works on a variable exactly as it does on a function,
class or type alias — `private count: int = 0` is emitted as `_count`, and so is
every reference to it in the module. a parameter, local or class attribute that
merely shares the name is a different binding and keeps it. reaching a private
symbol as an attribute of its module from another one (`helpers.count`) is
`inaccessible-member`, for the same reason importing it is `private-import`

the rename follows the symbol wherever the module binds it again — a `def` or
`class` of the same name, an import (`from m import count` becomes `from m import count as _count`), an `except ... as count`, a `match` capture, and a `global count`
in a function. a dotted `import count.sub` binds its top-level package, which no
alias can keep under `_count`, so it is an error. a private symbol is left out of
`from m import *`, and listing one in `__all__` is `private-export`

## private and protected

`private` and `protected` say who may reach a class member:

```by
class Account:
    protected rate: float = 0.05
    init(private let balance: int)

    def interest(self) -> float:
        return self.balance * self.rate


class Savings(Account):
    def bonus(self) -> float:
        return self.rate * 2        # `protected` — a subclass may
```

- **`private`** — only the declaring class's own body
- **`protected`** — that, and the body of any subclass

reaching one from anywhere else is `inaccessible-member`:

```by
def audit(account: Account) -> int:
    return account.balance  # error: `balance` is private to `Account`
```

the keywords work on every kind of member — a `def`, an attribute, a nested
class, a [property](properties.md), an [`init`](init-method.md) parameter — and
the member is written by the name it was declared with wherever it may be
reached. what changes is the name it is *emitted* under, because that is the only
enforcement python itself offers: `private` becomes `__name`, which python
name-mangles per class, and `protected` becomes `_name`, the convention python
uses for the same thing. a member no widened view can reach is also what
[safe variance](safe-variance.md) rests on

the mangled name is spelled out at the access site — `self.helper()` becomes
`self._A__helper()`. python mangles lexically, so a bare `self.__helper` written
in a nested scope would name whatever class encloses it, and the full spelling
names the same attribute from every one of them

a visibility keyword on a name python looks up verbatim — a dunder, or `_` — is
reported as having no effect. mangling applies only to a name with at most one
trailing underscore, so renaming would change what the member *is* rather than
who can reach it, and leaving it alone would make the modifier do nothing. the
one dunder where the keyword says something is
[`init`](init-method.md#private-constructors), which is checked at the
construction site instead

`protected` needs a class for the "and its subclasses" half to mean anything, so
it is only a modifier on a class member:

```by
protected def helper(): ...   # error: `protected` is only a modifier on a class member
```

a class variable takes one modifier besides its own `class` keyword — a
visibility keyword, which composes with any declaration. the class reaches it
through the class object as readily as through an instance:

```by
class Counter:
    private class var made: int = 0
    protected class let LIMIT: int = 3

    @classmethod
    def total(cls) -> int:
        return cls.made + cls.LIMIT
```

### visibility and inheritance

a visibility keyword decides the name the member is emitted under, so a member
emitted under a different name from the one it inherits does not override it.
it sits beside it, and the inherited member is still what a call finds — which is
never what the declaration looks like it does, so it is
`invalid-override-visibility`. that happens in two ways: a keyword declaring the
member narrower than what it inherits, and a plain declaration over an inherited
`protected` member

```by
class A:
    def f(self) -> int:
        return 1

    protected def g(self) -> int:
        return 1


class B(A):
    private def f(self) -> int:  # error: `f` is public on `A`
        return 2

    def g(self) -> int:          # error: `g` is protected on `A`
        return 2
```

a `private` member is the exception on the other side. two `private` members that
share a name are mangled apart, so neither overrides the other and neither has to
match the other's signature, and a subclass is free to declare a public member
under a name its base kept private:

```by
class Base:
    private def step(self, n: int) -> int:
        return n


class Derived(Base):
    def step(self) -> str:      # a new member, not a replacement
        return "x"
```

a `protected` member keeps one name across the hierarchy, so it overrides like
any other member and is checked like one

### names a member is looked up by

a visibility keyword renames every place a member is named, not only attribute
accesses: a bare name in the class body (`y = x + 1`, `@size.setter`), a string in
`__slots__` or `__match_args__`, and a keyword in a class pattern (`case Point(x=0)`). each is spelled the way that position needs — `__slots__` takes the
class-body spelling python mangles, `__match_args__` and a pattern keyword the one
`getattr` reaches

### where a visibility keyword cannot go

some names are how the class works at runtime, and renaming them would change what
the class does rather than who can reach it. a visibility keyword on one is
`invalid-visibility`:

- a field of a dataclass-like class, a named tuple or a typed dict — the field's
    name is its constructor's keyword, or its key
- an enum member — its name is how the enum looks it up
- an abstract method — a `private` one is renamed per class, so no subclass could
    ever override it

a keyword that cannot act on its name is `ineffective-private`: a dunder, `protected`
on a name that already starts with `__` (python mangles it, which makes it private),
and `private` in a class named only with underscores (python mangles nothing there)

a visibility keyword says who may reach a class member, or marks a module-level
declaration as the module's own. a declaration inside a function body is a local,
which nothing outside the function reaches anyway, so a keyword on one is an error

## inlay hints

a method that overrides a superclass member without saying so gets an
`override` inlay hint, written where the modifier would go. the hint navigates
to the superclass it overrides:

```by
class B(A):
    ⟨override ⟩def f(self): ...
```

constructor-like methods (`__init__`, `__new__`, `__post_init__`,
`__init_subclass__`) and name-mangled private methods are exempt, matching
`missing-override-decorator`
