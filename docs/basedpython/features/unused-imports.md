# unused imports

`by check` reports an import in a `.by` file that nothing uses, as `unused-import`, and
`by check --fix` removes it

```by
import json          # warning: `json` imported but unused
from enum import Enum

class Colour(Enum):
    RED = 1
```

an import is used when a name it binds is read — and also when the module it names declares an
[extension](extensions.md) or a conformance that supplies something the file relies on. an
extension's members are only applicable in the files that import its module, so an import can be
the reason `colour.lower_name` resolves without the module's name ever being written again

```by
# colours.by
from enum import Enum

extension Enum:
    let lower_name: str
        get() = self.name.lower()
```

```by
# main.by
import colours       # ok: the extension it declares is used below
from enum import Enum

class Colour(Enum):
    RED = 1

def describe(colour: Colour) -> str:
    return colour.lower_name
```

each of these counts as a use of the module that declares the extension or conformance:

- an extension member, read off a value or as a bare name in a
    [receiver block](implicit-receivers.md)
- an [operator](extensions.md#operators) or a [conversion](extensions.md#conversions) an extension
    supplies
- a conformance that makes a value acceptable where its protocol is asked for, or carries a
    protocol extension's members to it
- a [type test](extensions.md#the-type-test) answered from the conformance registry

a name the emitted python reads although the source never spells it is a use too, of the import
that binds it: the enum a bare `Red` or `case Red:` is lowered to qualify (`Color.Red`), and the
classes a type is written out with — the runtime check of a
[type test](parametric-type-tests.md), a [checked cast](checked-cast.md), a
[`raises`](exceptions.md) guard or a [soundness check](soundness.md), the specialization a
[reified](reified-generics.md) call or construction is given, and the bound of the type parameter
a union parameter is given. `x is AB`, with `type AB = A | B`, checks `A` and `B`. a soundness
check counts at every position, because which ones run is the build's choice

any import of the module makes its extensions applicable — `import colours`, `import colours.extra`
(for `colours` and `colours.extra`), and `from colours import something` — so each of them is used
when one of the module's extensions is, whether or not anything reads the name it binds. an import
makes them applicable anywhere in the file, so one written inside a function counts too

## the rules

apart from extensions, the rule is ruff's `F401`, which in turn is pyflakes':

- a read of a binding counts for every binding of that name after it in the scope, and a `del`
    starts the name afresh. an import nothing reads before the name is rebound is reported when
    another import rebinds it — what `F811` reports as a redefinition — so one round of fixes
    removes every one; an import a submodule import follows, or the other way round, is only
    redefined by an import of the same module. an import rebound by anything else is left to that
    rebinding
- an import in a class body is never reported, nor one that binds a `global` or `nonlocal` name
- `import a as a`, `from a import b as b` and [`from a export b`](export-imports.md) are re-exports,
    a name listed in `__all__` is exported, and `from __future__ import …` is a directive — none is
    reported
- a read inside a `TYPE_CHECKING` block counts, and an annotation may read an import written after
    it. a string in a type position is a [literal type](literal-types.md), so it reads nothing
- a file with a syntax error is not checked: the parser's recovery may have dropped a read

two kinds of unused import are reported without a fix:

- one in a package's `__init__.by`, which may be how the package exports the name. for one of the
    project's own modules the report says how to make the re-export explicit
- one in the body of a `try` that handles `ImportError`, which is usually a test for whether the
    module is installed. the report suggests `importlib.util.find_spec`

## buff

buff's `F401` does not check `.by` or `.byi` files. a linter reads one file at a time and has no
types, so it cannot see that an extension supplied something, and its fix would remove the import
that makes the extension applicable. `unused-import` is the rule there instead

that changes how a suppression is written. `# noqa: F401` in a `.by` file no longer suppresses
anything, and `RUF100`, where it is enabled, reports it as unused and removes it. an import that
needs to stay although nothing uses it is suppressed for `by check` instead:

```by
import side_effects  # ty: ignore[unused-import]
```

`by check --add-ignore` writes that comment on every unused import still reported.
`lint.pyflakes.allowed-unused-imports` is buff's setting and does not apply to `by check`; turn the
rule off for a set of files with a [per-path override](../configuration.md#per-path-overrides) instead

## related

- [extensions](extensions.md) — what an extension is, and how its import makes it applicable
- [linting](linter.md) — how ruff's other rules read `.by` source
