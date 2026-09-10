# basedpython: property syntax

basedpython gives classes Kotlin-style property syntax. A class-body `var` / `let` declaration
followed by an indented `get` / `set` / `field` accessor block becomes a python `@property` with a
backing field. Inside an accessor, `field` refers to the backing storage.

The parser lowers the whole construct to standard `@property` members and rewrites `field` to the
backing attribute, so ty's existing property handling applies unchanged — there is no special rule
for the `field` keyword.

Storage is an implementation detail: it is named `__<property>`, so python's name mangling hides it
and nothing outside the class can reach it. Inside the class you use the property's own name, which
reads as the storage type.

A backing field's initialiser is emitted into `__init__` (synthesized when the class has none), so
each instance gets its own storage. A declaration with no initialiser (`late field: T`) stays a
class-level annotation, which creates no runtime attribute and only declares the type.

## stored `var` property is readable and writable

```by
class Person:
    var age: int = 0
        get() = field
        set(value):
            field = value

p = Person()
reveal_type(p.age)  # revealed: int
p.age = 5
```

## storage does not exist under a reachable name

Storage is named `__<property>`, so python's name mangling hides it. There is no `_age` for anything
to reach, inside the class or out — the property's own name is the only way in.

```by
class Person:
    var age: int = 0
        get() = field
        set(value):
            field = value

    def f(self):
        reveal_type(self.age)  # revealed: int
        # error: [unresolved-attribute]
        print(self._age)

p = Person()
# error: [unresolved-attribute]
print(p._age)
```

## writing the wrong type to a property is an error

```by
class Person:
    var age: int = 0
        get() = field
        set(value):
            field = value

p = Person()
# error: [invalid-assignment]
p.age = "old"
```

## a `let` property with only a getter is read-only

```by
class Person:
    let name: str = ""
        get() = field

p = Person()
reveal_type(p.name)  # revealed: str
# error: [invalid-assignment]
p.name = "bob"
```

## an untyped property takes its type from its initialiser

a declaration that names no type but has an initialiser is the typed declaration with the type left
for the initialiser to say, the way any declaration's is: `var count = 0` is `var count: int = 0`.
the setter accepts that type and the property reads as it

```by
class Counter:
    var count = 0
        get() = field
        set(value):
            reveal_type(value)  # revealed: int
            field = value

c = Counter()
reveal_type(c.count)  # revealed: int
# error: [invalid-assignment]
c.count = "many"
```

## a getter is held to the type its initialiser declares

as it would be to a written type, so a getter that returns something else is an error rather than a
second type for the property to read as

```by
class Counter:
    var count = 0
        # error: [invalid-return-type]
        get() = str(field)
        set(value):
            field = value
```

## without an initialiser a property takes its type from the getter

only a `let` can leave both the type and the initialiser out. what `get` returns is the property's
type, as precisely as the accessor states it

```by
class A:
    let a
        get() = 1

reveal_type(A().a)  # revealed: 1
```

## a property declared `None` has no return annotation to remove

the declared type rides on the getter as its return annotation, but it is the property's type.
writing it is not the redundant `-> None` of a `def` that falls off its end

```by
class A:
    let n: None
        get(): pass
```

## a computed property has no backing storage

An accessor block that never mentions `field` allocates no backing field — the property is computed
from other state.

```by
class Rect:
    var w: int = 0
    var h: int = 0
    let area: int
        get() = self.w * self.h

r = Rect()
reveal_type(r.area)  # revealed: int
```

## a computed property takes no initialiser

an initialiser is stored in the backing field, and a property whose accessors never mention `field`
has none to store it in

```by
class Rect:
    var w: int = 0
    # error: [invalid-syntax] "a property with no backing `field` takes no initialiser"
    let area: int = 0
        get() = self.w * 2
```

## an explicit `field` declaration decouples storage from the public type

```by
from typing import Sequence

class Bag:
    let items: Sequence[int]
        field: list[int] = []
        get() = field

    def inside(self):
        reveal_type(self.items)  # revealed: list[int]

b = Bag()
reveal_type(b.items)  # revealed: Sequence[int]
```

## each instance gets its own backing storage

The point of putting the initialiser in `__init__`: a mutable implementation behind a read-only
public view must not be shared between instances.

```by
from typing import Sequence

class Bag:
    let items: Sequence[int]
        field: list[int] = []

    def add(self, n: int):
        # `items` reads as its storage type here, so no `_items` is needed
        self.items.append(n)

x = Bag()
y = Bag()
x.add(1)
assert list(x.items) == [1]
assert list(y.items) == []
```

## an explicit `field` declaration is a complete property

The getter is implicit — stating storage separately from the public type is the whole point, so it
needs no `get() = field` boilerplate.

```by
from typing import Sequence

class Bag:
    let items: Sequence[int]
        field: list[int] = []

reveal_type(Bag().items)  # revealed: Sequence[int]
```

## an unannotated `field` takes its type from the initialiser

Not from the property's public type — the two differing is the reason to declare storage separately.

```by
class A:
    let a: object
        field = 1

    def f(self):
        reveal_type(self.a)  # revealed: int
```

## an explicit `field` initialiser types the storage, not the property

only an initialiser written on the declaration itself states the property's type. one on an explicit
`field` states the storage's, so an untyped property still takes its type from the getter

```by
class Name:
    let length
        field = "ada"
        get() = len(field)

reveal_type(Name().length)  # revealed: int
```

## the property's type is the context an unannotated `field` is solved against

An initialiser that carries no type information of its own — a bare `[]` — is solved against the
property's declared type, so it lands on the storage type that type implies rather than falling back
to `Unknown`. The property's type does not *become* the storage type: storage stays `list`, only its
element type comes from the context.

```by
from typing import Sequence

class A:
    let a: Sequence[int]
        field = []

    def f(self):
        reveal_type(self.a)  # revealed: list[int]

def outside(x: A):
    reveal_type(x.a)  # revealed: Sequence[int]
```

## inside the declaring class a property reads at its storage type

The class works with the implementation type without naming `_a` everywhere; callers outside see the
public type. Reading is safe because the getter only reads the field, so both spellings denote the
same object.

```by
class A:
    let a: object
        field = 1

    def f(self):
        reveal_type(self.a)  # revealed: int

def outside(x: A):
    reveal_type(x.a)  # revealed: object
```

## a getter with logic keeps the public type inside the class

The narrow view is only sound when the getter is a pure field read; anything else has to keep being
called.

```by
class A:
    let a: object
        field = 1
        get():
            print("computed")
            return field

    def f(self):
        reveal_type(self.a)  # revealed: object
```

## a subclass sees the public type

The backing field belongs to the declaring class.

```by
class A:
    let a: object
        field = 1

class B(A):
    def f(self):
        reveal_type(self.a)  # revealed: object
```

## a write inside the class still goes through the setter

Only reads narrow — a write must not bypass a setter that validates.

```by
class A:
    var age: int = 0
        get() = field
        set(value):
            assert value >= 0
            field = value

    def bump(self):
        self.age = 1

a = A()
a.bump()
assert a.age == 1
```

## a `private` property is not reachable from outside its class

`private` renames a property the way it renames any member, so an access from outside the class is
reported: the property is emitted as `__x`, which Python name-mangles per class.

```by
class A:
    private var x: int = 0
        get() = field
        set(value):
            field = value

    def bump(self):
        self.x = self.x + 1
        reveal_type(self.x)  # revealed: int

a = A()
a.bump()
# error: [inaccessible-member]
print(a.x)
```

## a subclass cannot reach a `private` property

```by
class A:
    private var x: int = 0
        get() = field
        set(value):
            field = value

class B(A):
    def f(self):
        # error: [inaccessible-member]
        return self.x
```

## a write to a `private` property still runs the setter

A write inside the class goes through the property, not its storage, so validation is not bypassed.

```by
class A:
    private var x: int = 0
        get() = field
        set(value):
            assert value >= 0
            field = value

    def set_bad(self):
        self.x = -1

a = A()
validated = False
try:
    a.set_bad()
except AssertionError:
    validated = True
assert validated
```

## a `var` with only a getter still accepts writes

The pass-through setter is synthesized, so the property stays mutable.

```by
class A:
    var x: int = 0
        get() = field

a = A()
a.x = 3
reveal_type(a.x)  # revealed: int
```

## a suite on the accessor's line runs every statement

a suite may share the accessor's line, as it may a `def`'s. every statement in it runs, not only the
first

```by
class Account:
    var balance: int = 0
        get() = field
        set(value): checked = max(value, 0); field = checked

a = Account()
a.balance = -5
assert a.balance == 0
a.balance = 7
assert a.balance == 7
```

## accessor bodies are type-checked

The getter's body is a real method body, so a return that contradicts the declared property type is
reported.

```by
class B:
    let label: str
        get():
            # error: [invalid-return-type]
            return 1
```

## `override` on a property is checked against the base

```by
class Base:
    var age: int = 0
        get() = field
        set(value):
            field = value

class Child(Base):
    override var age: int = 0
        get() = field
        set(value):
            field = value

reveal_type(Child().age)  # revealed: int
```

## `override` on a property with no base member is an error

The `override` modifier reaches ty as a decorator on the accessor, so the usual check applies.

```by
class Base: ...

class Child(Base):
    # error: [invalid-explicit-override]
    override let age: int
        get() = field
```

## an abstract property declares a shape

```by
class Shape:
    abstract let area: int
        get() = field

reveal_type(Shape.area)  # revealed: property
```

## `late var` is the declared type, not an optional

`late` defers initialisation without widening the type — `handle` is `str`, not `str | None`.
Reading it before assignment raises `AttributeError` at runtime, exactly as for any unbound
attribute; the keyword is a type-checker hint.

Reading it before anything assigns it really does raise, so the type is checked here without
performing the read.

```by
class Loader:
    late var handle: str

def check(loader: Loader):
    reveal_type(loader.handle)  # revealed: str
```

## `late field` declares storage with no initialiser

```by
class Bag:
    let items: list[int]
        late field: list[int]
        get() = field

def check(bag: Bag):
    reveal_type(bag.items)  # revealed: list[int]
```

## `self` resolves inside an accessor body

```by
class A:
    var first: str = ""
    let shout: str
        get() = self.first.upper()

reveal_type(A().shout)  # revealed: str
```

## `static let` is a class-level computed property

python has no class-level `property` (chaining `classmethod` onto `property` was removed in 3.13),
so the construct lowers to a descriptor instead. it answers on the class and on an instance.

```by
class Config:
    static let name: str
        get() = "config"

reveal_type(Config.name)  # revealed: str
reveal_type(Config().name)  # revealed: str
```

## `static let` computes from the owning class

the getter receives the class, under the implicit name `cls`.

```by
class Widget:
    static let label: str
        get() = cls.__name__

reveal_type(Widget.label)  # revealed: str
```

## a `static` property is read-only and purely computed

a descriptor cannot intercept `A.x = v`, so the mutable and stored forms are rejected rather than
silently ignored.

```by
class A:
    # error: [invalid-syntax] "a `static` property is read-only; use `static let`"
    static var count: int
        get() = 1
        set(value):
            field = value
```

## a `static` property rejects a backing `field`

there is no per-instance slot for a class-level property to store in.

```by
class B:
    # error: [invalid-syntax] "a `static` property has no backing `field`"
    static let size: int
        # error: [invalid-syntax] "explicit `field` declaration is never referenced by an accessor"
        field: int = 0
        get() = 1
```

## `class let` is not the spelling for a property

`class` declares a class *variable*; the class-level property modifier is `static`, matching
`static def`.

```by
class C:
    # error: [invalid-syntax] "`class let` is not a property declaration; write `static let`"
    class let size: int
        get() = 1
```
