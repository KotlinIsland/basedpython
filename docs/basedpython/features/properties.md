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
        self._age: int = 0

    @property
    def age(self) -> int:
        return self._age

    @age.setter
    def age(self, value: int) -> None:
        assert value >= 0
        self._age = value
```

> **STATUS: planned for version 0.0.1a6, not yet implemented.** the `var`
> keyword, the `get`/`set` accessor block, `field`, `lateinit`, and the
> `let x: T` accessor form described below are not yet recognized by the
> parser. `let x: T = init` at class scope partially works as the
> [modifier](modifiers.md) form. tracking item: properties v0.0.1a6

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

| form                            | meaning                             |
| ------------------------------- | ----------------------------------- |
| `var x: T = init`               | mutable instance attribute          |
| `let x: T = init`               | read-only instance attribute        |
| `var x: T = init` + `get`/`set` | `@property` with backing field `_x` |
| `let x: T = init` + `get`       | `@property` with getter only        |

`let` at class scope used to lower to `x: Final = ...` (see [modifiers](modifiers.md)).
property lowering supersedes that: inside a class body `let x: T = init` now
emits a plain `self.x: T = init`, with read-only enforcement done by ty.
module-scope `let` is unaffected

## plain `var` / `let`

without accessors the keyword is stripped and the assignment lands in
`__init__`. no `Final` is emitted

```by
class Point:
    let x: int = 0
    var y: int = 0
```

transpiles to:

```python
class Point:
    def __init__(self) -> None:
        self.x: int = 0
        self.y: int = 0
```

ty sees `let` in the basedpython AST and records the attribute as read-only.
assignment to `self.x` outside the declaration site is reported. subclasses
may shadow `x` with their own declaration (mutable or immutable) — no
`Final` blocks them

if the class has an explicit `init(...)` (see
[init method shorthand](init-method.md)) the `var`/`let` assignments
prepend ahead of the user's body, after any `let`-parameter
self-assignments

## accessor block

accessor block is a suite directly following the declaration, indented one
level deeper. accepted entries: `get()`, `set(name)`, and `field`. each is
optional:

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
`self._<name>`. `field` only in scope inside accessor — referencing
elsewhere is parse error. assigning to `field` outside `set` rejected

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
    def __init__(self) -> None:
        self.w: int = 0
        self.h: int = 0

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
        self._items: list[int] = []

    @property
    def items(self) -> Sequence[int]:
        return self._items
```

rules:

- the declaration form is `field: <type> = <init>` or `field: <type>` (no
    initialiser, paired with `lateinit`)
- only one `field` declaration per accessor block
- accessors must reference `field` somewhere — otherwise an explicit
    backing field with no use is a parse error
- the property's own initialiser (`var x: T = init`) is rejected when an
    explicit `field` declaration carries its own initialiser. choose one site

shape mirrors Kotlin's explicit backing field proposal — the public type
and the storage type are stated independently, and `field` is typed by the
explicit declaration rather than inferred from the property

## lowering — accessor form

the accessor form lowers to a `@property` pair over a backing field, as in the
opening example

setter parameter annotation comes from the property's declared type.
getter return annotation matches. without an explicit `field` declaration,
backing field type also matches the property type

## modifiers

property declarations compose with [modifier keywords](modifiers.md):

| basedpython               | Python output                                  |
| ------------------------- | ---------------------------------------------- |
| `override var x: int = 0` | `x` overrides parent; `@override` on accessors |
| `final var x: int = 0`    | property marked `@final`                       |
| `abstract let x: int`     | `@property` + `@abstractmethod`, no body       |
| `private var x: int = 0`  | renamed to `_x` (backing `__x`)                |

`abstract let` / `abstract var` are bodyless. abstract `var` produces both
abstract getter and abstract setter

## `lateinit`

`lateinit var x: T` declares a property whose initialisation is deferred.
no initialiser, no accessor block:

```by
class Loader:
    lateinit var handle: File
```

transpiles to:

```python
class Loader:
    handle: File  # class-level annotation, no assignment
```

reading `handle` before assignment raises `AttributeError` at runtime —
same as ordinary unbound python attributes. `lateinit` is therefore a
type-checker hint: `handle` treated as `File` (not `File | None`) at use
sites, unassignment is the user's responsibility. only valid on `var`,
never on `let`

`lateinit` also accepted on a `field:` declaration when the property's
public form has no initialiser:

```by
class Bag:
    let items: Sequence[int]
        lateinit field: list[int]
        get() = field
```

## scope and placement

property declarations recognised only inside class body. `var` at module
scope is parse error (use plain assignment). `let` at module scope keeps
its existing [modifier-style meaning](modifiers.md) — module-level
constant, no property semantics

accessor blocks recognised only directly following a `let`/`var`
declaration. stray `get()` / `set(...)` elsewhere parses as normal call

## interaction with `init(...)`

`var` / `let` declarations and `init(let ...)` parameters coexist.
lowering order inside synthesised `__init__`:

1. `let`-parameter self-assignments from `init`
1. backing-field initialisers for accessor properties (`self._x = <init>`)
1. plain attribute initialisers for `var` / `let` without accessors
1. user-written body of `init`

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

transpiler synthesises a real `@property` descriptor for accessor-form
declarations, so ty's existing property handling applies unchanged. for
plain `var` / `let` the lowering produces ordinary `self.x: T = ...`
statements inside `__init__`, which ty already analyses for instance
attributes

read-only enforcement for `let` is implemented in ty by walking the
basedpython AST before lowering and recording attribute mutability per
class. no runtime annotation is emitted — the marker exists only in the
pre-lowering tree. subclasses are not blocked from overriding

`field` is rewritten to `self._<name>` before ty sees the lowered tree, so
ty never needs a special-case rule for it

## polyfill imports

property lowering injects, on demand, only what is used:

- `override` / `final` / `abstractmethod` as already documented in
    [modifiers](modifiers.md)

no `Final`, no `cached_property` — neither is emitted by this feature

## rejected forms

- `let` + `set` → parse error: "read-only property cannot define a setter"
- `lateinit let` → parse error: "lateinit requires var"
- `lateinit` with initialiser → parse error
- `field` referenced outside accessor body → parse error
- accessor block at module scope → parse error
- duplicate `get` / `set` / `field` in same accessor block → parse error
- explicit `field` with initialiser combined with property-side initialiser
    → parse error
- accessor block declaring explicit `field` but referencing it nowhere
    → parse error

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
