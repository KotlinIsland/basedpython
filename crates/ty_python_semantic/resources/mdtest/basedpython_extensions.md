# extensions

an extension adds methods and computed properties to an existing type without subclassing it. the
extended type's own type parameters are in scope under the names its declaration bound (`Element` on
`list`)

## a method on a builtin reuses its type parameter

```by
extension list:
    def second(self) -> Element:
        return self[1]

xs = [1, 2, 3]
reveal_type(xs.second())  # revealed: int

names = ["a", "b"]
reveal_type(names.second())  # revealed: str
```

## `self` in the body is the extended type

```by
extension list:
    def first_or(self, default: Element) -> Element:
        reveal_type(self)  # revealed: list[Element@list]
        if len(self) > 0:
            return self[0]
        return default
```

## a method may declare its own fresh type parameters

```by
extension list:
    def pair[R](self, other: R) -> tuple[Element, R]:
        return (self[0], other)

xs = [1, 2]
reveal_type(xs.pair("a"))  # revealed: (int, "a")
```

## extensions on a first-party class

```by
class Stack[T]:
    def __init__(self, items: list[T]) -> None:
        self._items = items

extension Stack:
    def peek(self) -> T:
        return self._items[-1]

s = Stack([1, 2, 3])
reveal_type(s.peek())  # revealed: int
```

## conditional extensions constrain the receiver

a bound on a reused parameter narrows where the extension applies

```by
extension list[Element: int]:
    def total(self) -> int:
        return sum(self)

ints = [1, 2, 3]
reveal_type(ints.total())  # revealed: int

flags = [True, False]
reveal_type(flags.total())  # revealed: int

words = ["a", "b"]
words.total()  # error: [unresolved-attribute]
```

## computed properties

```by
extension str:
    @property
    def shouty(self) -> str:
        return self.upper()

greeting = "hello"
reveal_type(greeting.shouty)  # revealed: str
```

## `static def` and `class def` members resolve on the class object

```by
extension str:
    static def joined(parts: list[str]) -> str:
        return "-".join(parts)

    class def empty(cls) -> str:
        return cls()

reveal_type(str.joined(["a", "b"]))  # revealed: str
reveal_type(str.empty())  # revealed: str
```

## extensions never shadow declared members

```by
class Widget:
    def label(self) -> str:
        return "real"

extension Widget:
    def label(self) -> int:
        return 0

w = Widget()
reveal_type(w.label())  # revealed: str
```

## importing a module makes its extensions applicable

`ext.by`:

```by
extension list:
    def second(self) -> Element:
        return self[1]
```

`main.by`:

```by
import ext

xs = [1, 2, 3]
reveal_type(xs.second())  # revealed: int
```

## without the import, the extension does not apply

`ext.by`:

```by
extension list:
    def second(self) -> Element:
        return self[1]
```

`main.by`:

```by
xs = [1, 2, 3]
xs.second()  # error: [unresolved-attribute]
```

## an ambiguous member is an error

```by
extension list:
    def twice(self) -> Element:
        return self[0]

extension list:
    def twice(self) -> Element:
        return self[1]

xs = [1, 2]
xs.twice()  # error: [ambiguous-extension-member]
```

## invalid declarations

the extended name must resolve to a class

```by
extension missing:  # error: [invalid-extension]
    def m(self) -> int:
        return 0
```

bracket parameters must reuse names the extended type declares

```by
extension list[T: int]:  # error: [invalid-extension]
    def total(self) -> int:
        return 0
```

an extension adds behaviour, not state

```by
extension list:
    count = 0  # error: [invalid-extension]
```

## an accessor-block property

a computed property may be written as an accessor block rather than a decorated `def`.

```by
class Box: ...

extension Box:
    let size: int
        get() = 1

reveal_type(Box().size)  # revealed: int
```

## a `static let` property reads off the class

a class-level computed property needs no descriptor here: the access site is rewritten, so the
backing function simply receives the class. it answers on an instance receiver too.

```by
class Widget: ...

extension Widget:
    static let kind: str
        get() = "widget"

reveal_type(Widget.kind)  # revealed: str
reveal_type(Widget().kind)  # revealed: str
```

## a `static let` property receives the extended class

```by
class Thing: ...

extension Thing:
    static let name: str
        get() = cls.__name__

reveal_type(Thing.name)  # revealed: str
```

## an instance property is not reachable on the class

only the class-level member kinds answer on a class receiver.

```by
class Crate: ...

extension Crate:
    let size: int
        get() = 1

# error: [unresolved-attribute]
reveal_type(Crate.size)  # revealed: Unknown
```

## a member is reachable from more than one narrowing statement

resolving an extension declaration infers module-level code, and that inference asks which
extensions apply — a re-entry the query recovers from rather than crashing on.

```by
extension list:
    def tagged(self) -> int:
        return 1

assert [1].tagged()
assert [1].tagged()

reveal_type([1].tagged())  # revealed: int
```
