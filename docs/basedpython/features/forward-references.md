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

a class's *subscript bases* evaluate while the class is being built, and so do
value-position subscripts in its body (`list[Tree]()`). a self-reference there
is quoted where it stands: `class Tree(list["Tree"])`. a direct base —
`class A(A)` — is *not* quoted. that is always a runtime error, and quoting it
would only mask the bug. method *bodies* are not rewritten either: by the time
a body runs, the class is bound, and quoting would produce a string instead of
a value

## why automatic

whether a name is bound by the time an annotation runs is a question about
the program's bindings, which the checker already answers. basedpython reads
every annotation as deferred, so you write the unquoted form everywhere and the
transpiler quotes exactly the references that need it

## converting python

in python a string annotation is a forward reference. in basedpython a string
in an annotation is a [literal type](literal-types.md), so
`by transpile --reverse` writes each one as the expression it spells:
`-> "Tree"` becomes `-> Tree`, and `"Tree | None"` becomes `Tree?`. the
arguments of `Literal[…]` and the metadata of `Annotated[…]` are values, and
stay strings

## when quoting is skipped

annotation quoting is only emitted when the annotation would otherwise be
evaluated eagerly. it is skipped when:

- the target is python 3.14 or newer — annotations are deferred natively
    (PEP 649), so the bare name resolves lazily
- the file already defers every annotation through
    `from __future__ import annotations`, whether you wrote it yourself or
    opted into the blanket injection

a subscript base evaluates eagerly on every target, so its self-reference is
quoted either way
