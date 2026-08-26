# properties

basedpython gives classes Kotlin-style property syntax. `var` and `let`
declare instance state with a single declaration site; custom `get`/`set`
accessors turn the declaration into a python `@property` with a backing
field

```by
class Person:
    var age: int = 0
        get() = field
        set(value):
            assert value >= 0
            field = value
```

transpiles to:

```python
class Person:
    def __init__(self) -> None:
        self.__age: int = 0
    @property
    def age(self) -> int:
        return self.__age
    @age.setter
    def age(self, value: int) -> None:
        assert value >= 0
        self.__age = value
```

> **STATUS: implemented, with one deliberate deviation from the shape originally
> sketched here.**
>
> - **a plain `var` / `let` declaration stays class-level.** only an accessor
>     property's backing-field initialiser moves into `__init__`; a declaration
>     without accessors is emitted as a class-body attribute, because dataclasses,
>     `TypedDict`, `Protocol`, enums, and the framework integrations all read
>     class-level annotations and would lose their fields otherwise. the
>     consequence is that `var items: list[int] = []` with no accessors is one
>     list shared by every instance — python's own semantics for what the user
>     wrote
>
> also not enforced: `field` referenced outside an accessor body is an ordinary
> identifier rather than a parse error, and a basedpython-only construct written
> *inside* an accessor body is not lowered — the accessor body is re-rendered
> from the AST, so the pipeline's final syntax check reports it instead of
> emitting bad python

without accessors the declaration stays a plain attribute — no descriptor
overhead

read-only-ness is a type-checker-only marker. no `Final` annotation is
emitted, so subclasses are free to override the property

## surface syntax

```by
class Person:
    var name: str = ""
    let id: int

    var age: int = 0
        get() = field
        set(value):
            assert value >= 0
            field = value
```

| form                            | meaning                                |
| ------------------------------- | -------------------------------------- |
| `var x: T = init`               | mutable instance attribute             |
| `let x: T = init`               | read-only instance attribute           |
| `var x: T = init` + `get`/`set` | `@property` with backing storage `__x` |
| `let x: T = init` + `get`       | `@property` with getter only           |
| `static let x: T` + `get`       | class-level property (descriptor)      |

`let` at class scope used to lower to `x: Final = ...` (see [modifiers](modifiers.md)).
property lowering supersedes that: inside a class body `let x: T = init` now
emits a plain `x: T = init`, with read-only enforcement done by ty.
module-scope `let` is unaffected

## plain `var` / `let`

without accessors the keyword is stripped and the declaration stays a class-body
attribute. no `Final` is emitted

```by
class Point:
    let x: int = 0
    var y: int = 0
```

transpiles to:

```python
class Point:
    x: int = 0
    y: int = 0
```

ty sees `let` in the basedpython AST and records the attribute as read-only.
assignment outside the declaration site is reported. subclasses
may shadow `x` with their own declaration (mutable or immutable) — no
`Final` blocks them

## accessor block

accessor block is a suite directly following the declaration, indented one
level deeper. accepted entries: `get()`, `set(name)`, and `field`. each is
optional:

- `field` alone → property with an implicit getter
- `let` with `get` only → read-only property
- `let` with `set` → parse error
- `var` with `get` only → property + pass-through setter
- `var` with `set` only → property + pass-through getter
- `var` with both → both accessors emitted

single-expression accessor uses `=`:

```by
get() = field * 2
```

multi-statement accessor uses `:` and a block:

```by
set(value):
    if value < 0: raise ValueError
    field = value
```

## `field` keyword

inside `get`/`set` body, `field` refers to backing storage. lowers to
`self.__<name>`. it is only meaningful inside an accessor; elsewhere it stays an
ordinary identifier (not currently a parse error — see the status note)

accessor that never references `field` allocates no backing storage —
property is computed. matches Kotlin's "no backing field" rule:

```by
class Rect:
    var w: int = 0
    var h: int = 0
    let area: int
        get() = self.w * self.h
```

transpiles to:

```python
class Rect:
    w: int = 0
    h: int = 0
    @property
    def area(self) -> int:
        return self.w * self.h
```

## explicit backing field

backing field type defaults to the property type. an explicit `field`
declaration inside the accessor block overrides both the type and the
initialiser of the backing storage. lets the public property expose a
narrower or wholly different type than the storage carries:

```by
class Bag:
    let items: Sequence[int]
        field: list[int] = []
        get() = field
```

transpiles to:

```python
class Bag:
    def __init__(self) -> None:
        self.__items: list[int] = []
    @property
    def items(self) -> Sequence[int]:
        return self.__items
```

rules:

- the declaration form is `field: <type> = <init>`, `field: <type>` (no
    initialiser, paired with `late`), or `field = <init>` — an unannotated
    declaration takes its type from the initialiser, *not* from the property's
    public type
- a `field` declaration on its own is a complete property: the getter is
    implicit. no `get() = field` boilerplate is needed
- only one `field` declaration per accessor block
- an accessor that was written must reference `field` somewhere — otherwise the
    explicit backing field is unused, which is a parse error. an implicit getter
    always reads it
- the property's own initialiser (`var x: T = init`) is rejected when an
    explicit `field` declaration carries its own initialiser. choose one site

shape mirrors Kotlin's explicit backing field proposal — the public type
and the storage type are stated independently, and `field` is typed by the
explicit declaration rather than inferred from the property

## storage type inside the class

inside the declaring class a property *reads* at its storage type rather than its
public one — the class works with the implementation without a second name for
storage, which is the reason to state the two types separately:

```by
class A:
    let a: object
        field = 1

    def f(self):
        reveal_type(self.a)   # int — the storage type

def outside(x: A):
    reveal_type(x.a)          # object — the public type
```

three rules keep this sound:

- only **reads** narrow. a write goes through the setter, which may validate
- only when the getter is a **pure field read** (implicit, or literally
    `get() = field`). a getter with logic has to keep being called, so it keeps
    the public type
- only in the **declaring class**. a subclass sees the public type

nothing changes in the emitted python: `self.a` stays `self.a` and calls the
getter, which returns exactly the backing field, so the narrower type cannot
disagree with the value

the property's own name is the only way to reach storage — write `self.a`, and
mutate through it directly:

```by
class Bag:
    let items: Sequence[int]
        field: list[int] = []

    def add(self, n: int):
        self.items.append(n)   # `items` is `list[int]` here
```

storage is named `__a`, so python's name mangling hides it: there is no `_a` for
anything to reach, inside the class or out. that also means a getter carrying
logic (which turns the narrow view off) leaves storage reachable only as
`self.__a` inside the class body

## lowering — accessor form

the accessor form lowers to a `@property` pair over a backing field, as in the
opening example

setter parameter annotation comes from the property's declared type.
getter return annotation matches. without an explicit `field` declaration,
backing field type also matches the property type

the backing field's initialiser is emitted into `__init__` — synthesized when the
class has none, otherwise injected ahead of the constructor's own statements — so
each instance gets its own storage. that matters because an explicit backing field
usually holds a mutable implementation behind a read-only public view, and a
class-body `__items: list[int] = []` would be one list shared by every instance

a declaration with no initialiser (a `late field: T`) stays a class-level
annotation, which creates no runtime attribute and only declares the type

two constructor shapes are not yet injected into and keep the class-level
declaration: a bodyless `init(...)`, whose body the
[init shorthand](init-method.md) lowering completes, and an inline
`def __init__(self): ...`. both need the two lowerings to agree on a single body

## modifiers

property declarations compose with [modifier keywords](modifiers.md):

| basedpython               | Python output                                  |
| ------------------------- | ---------------------------------------------- |
| `override var x: int = 0` | `x` overrides parent; `@override` on accessors |
| `final var x: int = 0`    | property marked `@final`                       |
| `abstract let x: int`     | `@property` + `@abstractmethod`, no body       |
| `private var x: int = 0`  | property renamed `_x`, storage `__x`           |

`abstract let` / `abstract var` are bodyless. abstract `var` produces both
abstract getter and abstract setter. the modifiers apply to a declaration that
carries an accessor block; `abstract let x: int` on its own (no accessors) is
still a plain annotated attribute, not a property

## `static` — a class-level property

`static let` declares a computed property on the *class*:

```by
class Config:
    static let default_name: str
        get() = "config"

print(Config.default_name)
```

the getter receives the owning class under the implicit name `cls`, the way an
instance accessor receives `self`:

```by
class Widget:
    static let label: str
        get() = cls.__name__
```

reading it works on the class and on an instance — `Config.default_name` and
`Config().default_name` both give `str`

python has no class-level `property`: chaining `classmethod` onto `property` was
deprecated in 3.11 and removed in 3.13. so the construct lowers to a small
read-only descriptor emitted into the preamble rather than to `property`:

```python
class _by_static_property:
    def __init__(self, fget):
        self._fget = fget
    def __get__(self, instance, owner=None):
        return self._fget(owner if owner is not None else type(instance))

class Config:
    @_by_static_property
    def default_name(cls) -> str:
        return "config"
```

a `static` property is read-only and purely computed. a descriptor's `__set__`
never fires for an assignment through the class (`Config.default_name = x`
rebinds the class attribute outright), and there is no per-instance slot for a
class-level property to store in, so `static var`, a `set` accessor, a `field`
declaration, and an initialiser are each rejected rather than silently ignored.
honouring any of them would mean installing a metaclass

inside an [extension](extensions.md) the same declaration needs no descriptor at
all — the access site is rewritten at transpile time, so the backing function
just receives the class

## `private`

`private` shifts the whole construct one level of underscore deeper — the property
becomes `_x` and its storage `__x`:

```by
class A:
    private var x: int = 0
        get() = field
        set(value):
            field = value

    def bump(self):
        self.x = self.x + 1
```

transpiles to:

```python
class A:
    def __init__(self) -> None:
        self.__x: int = 0
    @property
    def _x(self) -> int:
        return self.__x
    @_x.setter
    def _x(self, value: int) -> None:
        self.__x = value

    def bump(self):
        self._x = self._x + 1
```

accesses written inside the class under the public name are redirected, so the
declaration site is the only place the name changes. privacy is self-enforcing:
the property does not exist under its public name, so an access from outside the
class — or from a subclass — is an unresolved attribute, reported rather than
failing at runtime

a write is redirected to the property, not to its storage, so a validating setter
still runs

note this differs from a plain `private var x: int = 0` with no accessor block,
which is [stripped without renaming](modifiers.md) like any other class member
annotation — it is still private to the type checker, which is what
[safe variance](safe-variance.md) rests on, but nothing hides it at runtime

## `late`

`late var x: T` declares a property whose initialisation is deferred.
no initialiser, no accessor block:

```by
class Loader:
    late var handle: File
```

transpiles to:

```python
class Loader:
    handle: File  # class-level annotation, no assignment
```

reading `handle` before assignment raises `AttributeError` at runtime —
same as ordinary unbound python attributes. `late` is therefore a
type-checker hint: `handle` treated as `File` (not `File | None`) at use
sites, unassignment is the user's responsibility. only valid on `var`,
never on `let`

`late` also accepted on a `field:` declaration when the property's
public form has no initialiser:

```by
class Bag:
    let items: Sequence[int]
        late field: list[int]
        get() = field
```

## scope and placement

accessor blocks are recognised only inside a class body. `var` and `let`
themselves are declarations in every scope: outside a class they keep their
[modifier-style meaning](modifiers.md) — `let` is a module-level constant and
`var` a plain mutable declaration, neither with property semantics

accessor blocks recognised only directly following a `let`/`var`
declaration. stray `get()` / `set(...)` elsewhere parses as normal call

## interaction with `init(...)`

`var` / `let` declarations and `init(let ...)` parameters coexist. because
declarations and backing fields lower to class-body attributes rather than
constructor assignments (see the status note above), there is no ordering to
reconcile inside `__init__` — an `init(...)` body is untouched by this feature.
should storage move into the constructor later, that ordering becomes:
`let`-parameter self-assignments, then backing-field initialisers, then plain
`var` / `let` initialisers, then the user's `init` body

note: the `let` parameter modifier on `init` parameters is unrelated to the
class-body `let` property — it's the existing
[init shorthand](init-method.md) and continues to mean "self-assign this
parameter". no ambiguity since they appear in different positions

if a property's initialiser depends on a constructor parameter, the user
writes the assignment in the `init` body — declarations cannot reference
parameters:

```by
class Greeting:
    init(self, who: str):
        self.message = f"hello, {who}"
    let message: str
```

## ty integration

the parser synthesises a real `@property` descriptor for accessor-form
declarations, so ty's existing property handling applies unchanged — there is no
property-specific inference. for plain `var` / `let` the lowering produces an
ordinary class-body attribute, which ty already analyses

read-only enforcement for `let` reuses ty's `Final` machinery: the `let` marker
in the pre-lowering tree carries the `Final` qualifier, so a write away from the
declaration is reported as `invalid-assignment`. a `let` is exempt from the
override-of-final check, so subclasses are not blocked from overriding. no
runtime annotation is emitted — the marker exists only in the pre-lowering
tree

`field` is rewritten to `self.__<name>` by the parser, before ty sees the tree at
all, so ty never needs a special-case rule for it

## polyfill imports

property lowering injects, on demand, only what is used:

- `override` / `final` / `abstractmethod` as already documented in
    [modifiers](modifiers.md)

no `Final`, no `cached_property` — neither is emitted by this feature

## rejected forms

- `let` + `set` → parse error: "read-only property cannot define a setter"
- `late let` → parse error: "late requires var"
- `late` with initialiser → parse error
- `field` referenced outside accessor body → *not enforced*: it stays an
    ordinary identifier
- accessor block at module scope → parse error
- duplicate `get` / `set` / `field` in same accessor block → parse error
- explicit `field` with initialiser combined with property-side initialiser
    → parse error
- accessor block declaring explicit `field` but referencing it nowhere
    → parse error
- `static var`, or `static let` with `set` / `field` / an initialiser → parse
    error: a class-level property is read-only and purely computed
- `class let` / `class var` with an accessor block → parse error naming `static`
    as the modifier. the `class` keyword declares a
    [class variable](modifiers.md) (`class x = 1`, `class var x: T`) and a
    classmethod (`class def`), but not a property

## why

python's `@property` + `@x.setter` pair forces a four-line ritual for
every piece of validated state and physically separates getter from
setter. the Kotlin shape keeps property declaration, storage, and accessors
in one contiguous block. for the common case (plain attribute) basedpython
emits plain attribute — properties only show up when the user asks for
accessors

read-only via type-check-only marker (not `Final`) keeps subtyping open.
explicit backing field lets the exposed type and the storage type diverge
without hand-rolling a private attribute and a wrapper property
