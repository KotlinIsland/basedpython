# automatic forward references

an annotation can name a class defined further down, or the class it sits in:

```by
def root() -> Tree:
    return Tree()


class Tree(list[Tree]):
    children: list[Tree]

    def add(self, child: Tree) -> Tree:
        self.append(child)
        return child
```

before 3.14, python evaluates these annotations as the `def` or class body
runs, when `Tree` is not bound yet, and raises `NameError`. the conventional fix
is to quote the reference. basedpython does the quoting for you, so for a
target before 3.14 this transpiles to:

```python
def root() -> "Tree":
    return Tree()


class Tree(list["Tree"]):
    children: "list[Tree]"

    def add(self, child: "Tree") -> "Tree":
        self.append(child)
        return child
```

## scope

an annotation python evaluates as its definition runs is quoted when it names
something that is not bound by then: a class defined further down, the class
the annotation sits in, or a name imported only under `if TYPE_CHECKING:`,
which never runs at all. that covers parameter and return annotations, and the
annotations of class-body and module-level variables. the whole annotation is
quoted, so a basedpython type inside it (`Tree?`, `(Tree) -> None`) is quoted
in its lowered form (`"Tree | None"`, `"Callable[[Tree], None]"`)

a name that is already bound is left alone, as is a local variable's
annotation, which python never evaluates

method *bodies* are not rewritten: by the time a body runs, the class is bound,
and quoting would produce a string instead of a value

## outside an annotation

a type written as a value can name a later class too, wherever python only
records it as a type: the type arguments of a generic (`list[Tree]()`,
`Alias = dict[str, Tree]`, a base `Node[Tree]`), and the types a typing
construct assigned to a name takes as arguments (`TypeVar`, `NewType`, the
functional `NamedTuple` and `TypedDict`). a type argument that names one is
quoted whole, as an annotation is, with whatever it lowers to inside the quote:

```by
from typing import Callable, TypeVar

registry = dict[str, Plugin]()
Handler = Callable[[Plugin], None]
P = TypeVar("P", bound=Plugin)


class Plugin: ...
```

python evaluates these where they stand, on every version, so the reference is
quoted where it is written:

```python
from typing import Callable, TypeVar

registry = dict[str, "Plugin"]()
Handler = Callable[["Plugin"], None]
P = TypeVar("P", bound="Plugin")


class Plugin: ...
```

a string there is a [literal type](literal-types.md), as it is in an
annotation, so you write the forward reference as the bare name and never as a
string

some positions python reads as an object rather than as a type, and a quote
cannot stand in for one. a name not bound by then is an `unresolved-reference`
there:

- a base itself: `class A(A)`, or `class Bar(Plugin)` above `Plugin`. its type
    arguments are quoted as above (`class Tree(list[Tree])`)
- a keyword the class is built with, such as `metaclass=Meta`. a `TypedDict`'s
    `extra_items=` is a type, and is quoted as above
- the type arguments of a [reified](reified-class-generics.md) class, which keeps
    them as runtime values, wherever it stands: `list[Box[Plugin]]` too
- the type an `is`, `cast!` or `cast?` checks for, and the type `cast` reads
- a value inside a type: the arguments of `Literal[…]` (an enum member) and
    the metadata of `Annotated[…]`

## why automatic

whether a name is bound by the time an annotation runs is a question about
the program's bindings, which the checker already answers. basedpython reads
every annotation as deferred, so you write the unquoted form everywhere and the
transpiler quotes exactly the references that need it

## converting python

in python a string annotation is a forward reference. in basedpython a string
in an annotation is a [literal type](literal-types.md), so
`by transpile --reverse` writes each one as the expression it spells:
`-> "Tree"` becomes `-> Tree`, and `"Tree | None"` becomes `Tree?`. the same
goes for a string outside an annotation that names a type: `List["Tree"]`
becomes `List[Tree]`, and `TypeVar("T", bound="Tree")` becomes
`TypeVar("T", bound=Tree)`. the arguments of `Literal[…]` and the metadata of
`Annotated[…]` are values, and stay strings, as does a subscript that is a
lookup rather than a generic (`Colour["Red"]` on an enum)

the type `cast` reads is read where it is written, so it has no spelling for a
class defined further down: `cast("Later", o)` above `class Later` comes back
as `cast(Later, o)`, which the checker reports as an `unresolved-reference`.
move the cast below the class

## when quoting is skipped

annotation quoting is only emitted when the annotation would otherwise be
evaluated eagerly. it is skipped when:

- the target is python 3.14 or newer — annotations are deferred natively
    (PEP 649), so the bare name resolves lazily
- the file already defers every annotation through
    `from __future__ import annotations`, whether you wrote it yourself,
    opted into the blanket injection, or target a version below 3.10, which
    always gets it

a type outside an annotation evaluates where it stands on every target, so a
forward reference in one is quoted either way
