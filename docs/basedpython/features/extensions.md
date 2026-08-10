# extensions

an extension adds methods and computed properties to an existing type without
subclassing it or touching its definition, and declares the interfaces that type
[conforms to](#conformance):

```by
extension list:
    def second(self) -> Element:
        return self[1]
```

`xs.second()` is then available on any `list`, including the builtin one. the
extension reuses `list`'s own type parameter `Element` — it does not declare a
new one — so the return type tracks the element type with no extra ceremony

a method may reuse the extended type's parameters directly, by the names its
declaration gave them, and may also introduce its own fresh parameters:

```by
extension list:
    def foo[R](self, r: R) -> Element | R: ...
```

`Element` is not a new parameter and not a sigil — it is the name `list`'s
declaration bound (`class list[in out Element]` in basedpython's typeshed).
`[R]` is a fresh parameter the method declares normally

## reusing declared parameters

inside an extension, the extended type's parameters are in scope under the
names its declaration used. for a first-party class the names are yours:

```by
class Stack[T]:
    _items: list[T]

extension Stack:
    def peek(self) -> T:
        return self._items[-1]
```

for a typeshed type the names follow basedpython's typeshed, which spells the
core containers `list[Element]`, `dict[Key, Value]`, `set[Element]`:

```by
extension dict:
    def invert(self) -> dict[Value, Key]: ...
```

referencing a name the extended type did not declare is an error — there is no
implicit free-parameter introduction, which is exactly what keeps the
"used before defined" case unreachable

## conditional extensions

a bound on a reused parameter narrows where the extension applies. it does not
re-declare the parameter — it constrains the receiver:

```by
extension list[Element: int]:
    def total(self) -> int:
        return sum(self)
```

`total` is visible on `list[int]` and `list[bool]`, not on `list[str]`. the
bound is spelled with basedpython's existing bracket-bound syntax so it reads
like every other bound in the language. parameters left out of the bracket stay
reused, unconstrained

constraint applicability is resolved by the type checker per call site, so an
extension can overlap a builtin or another extension and only apply to the
arm that satisfies its bound

## what an extension may add

extensions add behaviour, not state — methods, `class def`/`static def`
methods, and computed [properties](properties.md). they may not add stored
fields, because there is nowhere to store them on an already-constructed
instance of a builtin, and it keeps the feature implementable without touching
object layout

```by
extension str:
    @property
    def shouty(self) -> str:
        return self.upper()
```

a computed property may equally be written as a [property](properties.md)
accessor block, including the class-level `static let` form:

```by
class Widget: ...

extension Widget:
    let size: int
        get() = 1
    static let kind: str
        get() = "widget"
```

a `static let` in an extension needs none of the descriptor machinery the same
declaration requires in a plain class: the access site is rewritten at transpile
time, so `Widget.kind` simply passes the class to the backing function

## operators

an extension may supply an operator's dunder, which makes the operator itself
available on the extended type:

```by
class Money:
    cents: int

extension Money:
    def __add__(self, other: Money) -> Money:
        return Money(self.cents + other.cents)

    def __neg__(self) -> Money:
        return Money(-self.cents)

    def __lt__(self, other: Money) -> bool:
        return self.cents < other.cents
```

`a + b`, `-a` and `a < b` then type-check, and each lowers to its backing
function the same way a named member does — `a + b` →
`_by_ext__Money____add__(a, b)`

the supported set is the operators python itself resolves through a dunder on
its operands:

| form                              | dunder                                             |
| --------------------------------- | -------------------------------------------------- |
| `-a`, `+a`, `~a`                  | `__neg__`, `__pos__`, `__invert__`                 |
| `a + b` and every binary operator | `__add__` …, and the reflected `__radd__` … on `b` |
| `a < b`, `a == b`, …              | `__lt__`, `__eq__`, …                              |
| `a in b`, `a not in b`            | `__contains__` on `b`                              |

precedence is the same rule every extension member follows: the operand's own
dunder wins, and an extension only answers where nothing else does. `str`
declares `__add__`, so an `extension str: def __add__` is never reached —
extensions are purely additive here too

three forms are deliberately left out, because there is no lowering that
preserves what the syntax means:

- a comparison **chain** (`a < b < c`) — two calls joined by a short-circuit
- an **augmented assignment** (`a += b`) — rewriting it re-evaluates the target
- every dunder outside the table (`__call__`, `__iter__`, `__enter__`, …) —
    reachable by name, not through the syntax that would normally invoke it

each of those keeps reporting the operator as unsupported, so the checker and
the runtime never disagree. write the call out, or the two comparisons

## conversions

the [conversion dunders](conversions.md) are the one family outside the operator
table that an extension can supply and have the *language* reach for you:

```by
extension Path:
    class def __of__(cls, value: str) -> Path:
        return Path(value)

p: Path = "/tmp/x"
```

so a type you do not own can be given a conversion from a literal, or from
another type. see
[conversions from an extension](conversions.md#conversions-from-an-extension)

## unqualified inside a block

a [trailing lambda](trailing-lambdas.md) block whose callback declares an
[implicit receiver](implicit-receivers.md) puts that receiver's members in scope
unqualified. an extension of the receiver's type supplies members too, so they
resolve there the same way:

```by
extension Tag:
    def p(self, block: Tag.() -> None): ...

doc.div:
    p:                  # the extension's member, reached with no `self.`
        text("hello")
```

reached last, after the receiver's own members and after anything the lexical
chain binds — the same precedence every other extension lookup follows. this is
what lets a third party add builders to a type from their own package and have
them resolve inside a block

## conformance

an argument list on an extension declares that the extended type *conforms* to
those interfaces. the block supplies whatever the interface asks for and the
type does not already answer:

```by
protocol Show:
    def show(self) -> str

extension str(Show):
    override def show(self) -> str:
        return self

def render(value: Show) -> str:
    return value.show()

render("hi")
```

neither side has to be yours. the interface may come from one dependency and the
type from another, and the conformance lives in your module

the interface must be a **protocol**. an abstract class carries concrete methods
a conformance could never answer — nothing would put them in the witness table —
and it already has inheritance and `register` for the job

### what has to be supplied

every member the interface declares, unless something else already answers it:

- a member of the conformance block itself
- a default on an [extension of the interface](#extending-an-interface)
- a member the type already has

anything left over is `invalid-conformance`, reported at the header — a
requirement nothing answers is an `AttributeError` the first time something
dispatches through the conformance

a conformance block may also add members that are *not* interface members. they
are inherent members of the extended type like any other extension's, and the
interface knows nothing about them

### extending an interface

an extension of a protocol adds members to every type that conforms to it:

```by
protocol Show:
    def show(self) -> str

extension Show:
    def shout(self) -> str:
        return self.show().upper()

extension str(Show):
    override def show(self) -> str:
        return self

"hi".shout()        # `str` conforms, so it has `shout`
```

a member here whose name matches a requirement is the *default* for it: a
conformance that does not supply that member inherits this one

### the type test

`is` answers from the conformances in scope, so a conforming value tests
positive even though it is not a subclass of anything:

```by
def describe(value: object) -> str:
    if value is Show:
        return value.shout()
    return ""

describe("hi")
```

nothing is wrapped. the value inside the branch is the value that went in — same
identity, same `type()`, same hash — so a conformance costs nothing at the
boundary and mutation through it is mutation of the object itself

### what a conformance may not do

three shapes are rejected rather than half-supported, because nothing in the
lowering could carry them:

- **a bracket bound.** conformance is registered per class, so a bound could not
    be checked where a value is dispatched on — `list[str]` would be handed the
    `list[int]` witness. declare the members in a bounded `extension` and conform
    the type unconditionally
- **supplying a dunder.** an ordinary extension may supply an
    [operator's](#operators) dunder, because that rewrite happens at the use site
    from the *concrete* operand type — but a requirement is reached through the
    interface, where the concrete type is precisely what is unknown, and python
    resolves a dunder on the type rather than through an attribute access. a type
    that *already* has the dunder needs no witness for it and conforms fine
- **naming a type declared further down the file.** a conformance registers
    itself where it is written, so both the protocol and the type have to exist
    by then

## implicit imports

importing a module makes its extensions and conformances applicable — there is
no per-extension import. either import form is enough for the type checker to
consider everything `mod` defines:

```by
import textwrap

# textwrap's extensions on `str` are now in scope
greeting.dedented()
```

naming what a module declares is the usual way a file depends on it — a
conformance is written against an interface imported by name — so
`from mod import Show` carries `mod`'s extensions too. requiring a separate
`import mod` whose symbols were never used would leave an import that reads as
removable to anyone tidying the file, silently withdrawing conformance

nothing is registered globally and nothing is monkeypatched, so two dependencies
cannot fight over the same pair: their conformances are simply not visible to
each other

two conformances of the same pair *reaching one file* is an error, reported at
the declaration that brings the second into view — which witness table survived
would otherwise depend on import order

a module *declaring* a conformance is imported eagerly even where imports are
otherwise deferred: the registration is the point of that import, and deferring
it would defer the conformance out of existence. nothing else gives up laziness
— a module carrying only ordinary extensions is resolved at transpile time and
needs nothing to have run, and a module that merely imports a conforming one is
not somewhere any conformance is applicable

the transpiler wires up the runtime side automatically (see below), so
`import mod` carries the extensions without `from mod import dedented` ever
being written by hand

## lowering

python has no extension methods, and builtin C types cannot be monkey-patched
at runtime, so extensions are resolved entirely at transpile time — no runtime
machinery, the same approach the rest of basedpython takes

each extension member lowers to a module-level free function whose first
parameter is the receiver. annotations are dropped from the backing function
— they reference type parameters with no runtime binding at module level —
and a marker comment carries the member kind and the original header
(bounds included) as provenance for the reverse transpiler:

```by
extension list:
    def second(self) -> Element:
        return self[1]
```

→

```python
def _by_ext__list__second(self):  # basedpython: extension method list
    return self[1]
```

the name carries a single leading underscore deliberately: python applies
private-name mangling to any `__name` reference inside a class body, so a
double-underscore backing name would break an extension call written in one

when a module declares more than one extension of the same target, later
ones mangle with an ordinal (`_by_ext2__list__…`) so their members don't
collide — conditional extensions of the same method name coexist this way

call sites are rewritten by the type checker. ty already knows the receiver's
type and which extensions are in scope, so `xs.second()` resolves to the
backing function and lowers to a plain call:

```by
xs.second()
```

→

```python
_by_ext__list__second(xs)
```

computed properties lower the same way, minus the call parentheses:
`name.shouty` → `_by_ext__str__shouty(name)`

a `static let` property passes the class rather than an instance, so
`Widget.kind` → `_by_ext__Widget__kind(Widget)`. read through an instance it is
widened the way a `class def` receiver is: `Widget().kind` →
`_by_ext__Widget__kind(type(Widget()))`

because the rewrite is type-directed, an extension call is never confused with a
real attribute. a method that happens to share a name with a real attribute
loses to the real attribute — extensions never shadow declared members

### implicit imports, lowered

when a call site uses an extension defined in another module, the lowering emits
the precise import of the backing function into the output, keyed off ty's
resolution of the call site:

```by
import textwrap

greeting.dedented()
```

→

```python
from textwrap import _by_ext__str__dedented

_by_ext__str__dedented(greeting)
```

so the surface stays `import textwrap`, and only the functions actually used are
imported — the implicit-import convenience costs nothing at runtime

### conformance, lowered

a conformance is the one part that needs a runtime: `str` cannot be
monkey-patched, and nothing about the value records what it conforms to. so the
block registers a *witness table* — each requirement mapped to the function that
answers it — against the pair, when its module is imported. the registry is one
per process, so a conformance registered by any module is visible to every
other:

```by
extension str(Show):
    override def show(self) -> str:
        return self
```

→

```python
def _by_ext__str__show(self):  # basedpython: extension method str(Show)
    return self

_by_conform(Show, str, {"show": _by_ext__str__show})
```

a requirement the type already answers is left out of the table, and a
requirement answered by a default on the interface's own extension maps to that
extension's backing function

the table is read at exactly two places. a requirement accessed on a receiver
the checker typed as the *interface* cannot be a plain attribute — the value may
be a conforming type that carries no such member — so it always goes through the
dispatcher, which falls back to the attribute when nothing registered one. it
dispatches whether or not a conformance is visible *here*, because a conformance
is written in the module that imports the interface and so is never visible to
the module that declares the function using it:

```by
def render(value: Show) -> str:
    return value.show()
```

→

```python
def render(value):
    return _by_witness(value, Show, "show")()
```

and `value is Show` answers from the table first, falling back to checking that
the value carries the requirements

everything else stays static. a member reached on a receiver whose concrete type
the checker knows — `"hi".show()` — is the same direct call to the backing
function any other extension member lowers to, with no lookup at all, and so is
an inherent member of an interface's extension

## round-tripping

the reverse transpiler re-sugars both halves from the marker-comment
provenance: a backing function tagged `# basedpython: extension …` becomes an
`extension` block (consecutive same-header functions share one block), and a
call of a same-file backing function becomes receiver-method form —
`_by_ext__list__second(xs)` → `xs.second()`, property calls drop back to
bare attributes, `functools.partial(…, xs)` references to `xs.second`.
backing-shaped functions and calls written by hand without the marker are
left as ordinary python, and a call of a backing function *imported* from
another module conservatively stays as the explicit call. a witness-table
registration is dropped: it is the lowering of the conformance list the marker's
header already carries

## current limitations

- an extension member cannot be reached through an optional chain
    (`xs?.second()`) yet — the transpiler reports an error rather than emit
    code that breaks the chain's short-circuit
- extension members resolve on a single receiver type, not across a union
- an unapplied method reference (`f = xs.second`) lowers to
    `functools.partial`, which binds the receiver eagerly like a bound method
    but is not one at runtime
- besides the [operator](#operators) dunders and the
    [conversion](#conversions) ones, every other dunder is reachable by name but
    not through the syntax that invokes it, and a comparison chain is not
    rewritten
- a conformance repairs an assignment at the positions the type checker checks
    a value against a declared type — an argument, an annotated assignment, a
    `return`, an element of a collection literal. it does not reach inside an
    already-constructed generic, so a `list[str]` is not a `list[Show]` even
    where `str` conforms; write the conversion where it happens
- a conflict between two conformances neither of which can see the other — two
    dependencies conforming the same pair, brought together by a third file —
    is not reported yet, and the last module imported wins at runtime
- a requirement cannot be reached through an optional chain (`value?.show()`)
    or assigned through the interface; both are reported rather than lowered
- an unused `import` that carries a conformance is still reported by `F401`,
    whose autofix would remove it and silently withdraw the conformance
