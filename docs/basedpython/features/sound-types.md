# sound types

python's [gradual guarantee] requires a type checker to fall back to a gradual type
(`Any` / `Unknown`) whenever an annotation is missing, even when a precise type could be
inferred. adding annotations must never introduce new errors, so an unannotated symbol has to
accept anything

in a fully typed project that trade is all cost and no benefit. it forces an annotation to be
written for something the checker already knows, and it silently swallows real mistakes

```python
def f(a=1):
    ...
f("nonsense")  # no error: `a` is gradual, so anything goes
```

the `analysis.sound-types` option deliberately breaks the gradual guarantee and uses the precise
type instead

```toml
[analysis]
sound-types = true
```

an explicit annotation always wins over anything inferred by this option

## per-module configuration

the option is resolved per module, so it can be adopted incrementally. use an override to enable it
for part of a project

```toml
[[overrides]]
include = ["src/core/**"]

[overrides.analysis]
sound-types = true
```

the rule is that **the module declaring a construct governs how that construct's types are
inferred**, and consumers see the result whatever their own setting is. so a sound module's
signatures stay precise when a gradual module imports them

```python
# src/core/lib.py  — sound
def f(a=1) -> None: ...

# src/legacy/main.py  — gradual
from src.core.lib import f
f("wrong")  # error: `f` is declared in a sound module, so its signature is precise here too
```

and the reverse holds: a sound module importing a gradual one does not retroactively tighten it

```python
# src/legacy/lib.py  — gradual
def g(a=1) -> None: ...

# src/core/main.py  — sound
from src.legacy.lib import g
g("fine")  # no error: `g` is declared in a gradual module
```

this applies uniformly. a parameter default, an inherited override signature and a `ClassVar` are
governed by the module that declares them; a lambda or a collection literal by the module it is
written in; and a call's type-variable solution by the module declaring the callable, so a sound
module never re-interprets a gradual module's generics

## parameter defaults

an unannotated parameter with a default is declared with the default's promoted type

```python
def f(a=1):
    reveal_type(a)  # int

f(2)      # ok
f("x")    # error: invalid-argument-type
```

lambdas follow the same rule, and unlike a `def` there is nowhere to write the annotation at all

```python
g = lambda a=1: a
g("x")    # error: invalid-argument-type
```

a `Callable` type context still takes priority over the default

```python
cb: Callable[[str], str] = lambda a="s": a  # `a` is `str`
```

## unannotated overrides

an unannotated method inherits the parameter and return types of the method it overrides. the
lookup starts *after* the class itself — the same walk `super()` performs — so it finds what is
being overridden rather than the method being defined

```python
class Base:
    def m(self, a: int, b: str = "x") -> bytes: ...

class Sub(Base):
    def m(self, a, b="y"):
        reveal_type(a)  # int
        reveal_type(b)  # str
        return b""

Sub().m("nope")  # error: invalid-argument-type
```

`Protocol` members and `abstractmethod` declarations are ordinary base methods for this purpose

a method that overrides nothing stays gradual. the base signature is also ignored when it mentions
a type variable (including an implicit `Self`), because those are bound to the base method's own
scope and copying them across would silently rebind them

## bare `ClassVar`

a bare `ClassVar` uses its inferred type. without this, adding `ClassVar` — a strengthening of
intent — would *degrade* the type relative to writing nothing at all

```python
class C:
    x: ClassVar = 1   # int   (was `Unknown | Literal[1]`)
    y = 1             # int
```

## empty collection literals

an empty collection literal has element type `Never`, so `first([])` solves `T` to `Never`
instead of leaking `Unknown`. a non-empty literal is unaffected

a type variable a call leaves *unsolved* is a separate matter, and is covered by
[precise unsolved type variables](precise-unsolved-typevars.md), which is on by default

## not covered

these are known gradual-guarantee costs that `sound-types` does **not** currently address

- **unannotated return types** are never inferred: `def f(): return 1` is `-> Unknown`. this is the
    largest remaining source of `Unknown` in a typed project, and is planned upstream
- **unannotated function decorators erase the signature**: under `@deco` where `deco` is
    unannotated, the decorated function becomes `Unknown` and its call sites go unchecked. ty already
    preserves the decorated object through an unannotated *class* decorator; the function case should
    follow
- **singleton promotion in collection literals**: `[None]` is `list[None | Unknown]`. fluid
    specializations now cover what this promotion was for
- **implicit attribute collections across methods**: `self.items = []` in `__init__` plus
    `self.items.append(v)` elsewhere gives `list[Unknown]`; fluid specializations stop at the scope
    boundary
- **unannotated `*args` / `**kwargs`** are `tuple[Unknown, ...]` / `dict[str, Unknown]`, and a
    `target(**kwargs)` forward is entirely unchecked
- **`Any` from typeshed and third-party stubs** (`json.loads`) stays gradual

## known rough edges

- a `None` default declares `None`, so `def f(a=None)` rejects `f(1)`. write
    `a: int | None = None` for the usual idiom
- a collection default goes through literal promotion rather than the collection-literal path, so
    `def g(a=[])` is `list[Unknown]` and `def h(a=())` is `tuple[()]`

[gradual guarantee]: https://typing.python.org/en/latest/spec/concepts.html#the-gradual-guarantee
