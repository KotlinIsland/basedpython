# export imports

`from x export y` is the re-exporting form of `from x import y`

```by
from collections.abc import Sequence
from .models export Widget
```

transpiles to:

```python
from collections.abc import Sequence
from .models import Widget as Widget
```

`name as name` is python's explicit re-export convention: it marks the binding
as deliberately part of the importing module's public api rather than an
implementation detail it happens to need. `export` says the same thing without
writing the name twice

## why it matters

in a stub file a plain `import` is private. importing through it is an error,
while an exported name is visible to the outside:

```by
# lib.byi
from typing import Any        # private to the stub
from .impl export Widget      # part of lib's api
```

```by
# main.by
from lib import Widget        # ok
from lib import Any           # error: module `lib` has no member `Any`
```

the same distinction drives ruff's unused-import rule (`F401`): an exported
name is never reported as unused, because nothing local is meant to reference
it

## restrictions

`export` binds each name under itself, so two forms have no meaning and are
parse errors:

| form                   | why                                                     |
| ---------------------- | ------------------------------------------------------- |
| `from x export a as b` | a rename binds a *different* name — use `import … as …` |
| `from x export *`      | a star binds no single name                             |

a relative import may omit its module, so `from . export y` is a module-less
relative export. write `from . export import y` (or `from .export import y`) to
import from a module actually named `export`

## reverse transform

`by reverse` rewrites the python spelling back:

```python
from x import a as a, b as b
```

becomes

```by
from x export a, b
```

it only fires when *every* alias in the statement is a redundant alias. a mixed
statement (`from x import a as a, b`) has no single-keyword form, so it is left
as written

## related

- [modifiers and visibility](modifiers.md) — the `export` / `private` modifier
    on declarations, which feeds the generated `__all__`
- [lazy imports](lazy-imports.md) — `lazy from x export y` composes as expected
