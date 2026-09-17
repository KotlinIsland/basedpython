# lazy imports

every `import` and `from import` statement in a `.by` file is automatically
marked lazy. the transpiler prepends the `lazy` keyword
([PEP 810](https://peps.python.org/pep-0810/), Python 3.15+) so the
runtime defers module loading until first use

```by
import os

print(os)
```

transpiles to:

```python
lazy import os

print(os)
```

Python 3.15's runtime registers `os` in `sys.modules` immediately but
defers executing its body until the first attribute access on the module
object. accessing `os.sep` (or `print(os)`, which calls `__repr__`) is
what actually loads the module

## supported forms

| basedpython                   | Python output                      |
| ----------------------------- | ---------------------------------- |
| `import os`                   | `lazy import os`                   |
| `import os as o`              | `lazy import os as o`              |
| `import os.path as p`         | `lazy import os.path as p`         |
| `from os import path`         | `lazy from os import path`         |
| `from os import path as p`    | `lazy from os import path as p`    |
| `from os import path, getcwd` | `lazy from os import path, getcwd` |
| `from .pkg import x`          | `lazy from .pkg import x`          |

`import a.b` without an alias stays eager (write `import a.b as ab` to opt
in). `from __future__ import …` and `from x import *` are always eager

a stub is never executed, so it has nothing to defer: a `.byi` keeps every import
as written, less any `lazy`

## target version

on python 3.15 and later, the PEP 810 `lazy` keyword is used directly.
on older runtimes, a runtime polyfill is emitted. `from __future__` and
`from x import *` are always left eager.

the polyfill uses `sys` and `importlib` itself, so importing either stays eager
under it — including as one name of a multi-name `import math, sys, time`

set the target with `--min-version 3.15` on `by transpile`/`by build`/`by run`
(`by check` uses `--python-version` for the same concept)

## the polyfill (python < 3.15)

without the `lazy` keyword there is no language-level way to defer a binding,
so the polyfill emits two different shapes:

- `import os` → `os = _lazy_module("os")`, which wraps the loader in
    `importlib.util.LazyLoader` and hands back a **real module object**. nothing
    is proxied, so this form behaves exactly like an eager import apart from when
    the body runs
- `from os import path` → `path = _lazy_attr("os", "path")`, a `_LazyAttr`
    **proxy**. a proxy is unavoidable here: `_lazy_module` already defers the
    module body, but reading `path` off the module would force it to run
    immediately, which is the very thing being deferred

the proxy is transparent — it forwards the operators (`==`, `+`, `len`, `in`,
`str`, `hash`, comparisons, …) to the value behind it, so it behaves like the
imported object. this matters more than it sounds: python looks special methods
up on the *type* and never routes them through `__getattr__`, so any dunder the
proxy fails to forward silently falls back to `object`'s version rather than
raising — an unforwarded `__eq__` would make `a == b` compare proxy identity
and quietly answer `False` for equal values

`isinstance` works in both directions: `isinstance(x, C)` for a proxied `x`
(via `__class__`), and `isinstance(x, C)` where `C` itself was lazily imported
(via `__instancecheck__`)

an **exception class is never proxied**. `except` is the one place cpython
refuses a stand-in: it checks that what it catches is a real class inheriting
`BaseException`, and never consults `__instancecheck__`. so
`from errors import ParseError` stays an ordinary import, and the rest of the
statement's names still don't:

```py
from errors import ParseError
decode = _lazy_attr("errors", "decode")
```

nor is a **special form** — `ClassVar`, `Final`, `Literal`, `Annotated`, `Protocol` and
the rest of what a type checker gives a meaning of its own. what reads one tells it
apart by identity: `dataclasses` decides a bare `ClassVar` annotation is not a field
by asking whether it *is* `typing.ClassVar`, and a checker reading the emitted python
rejects `ClassVar[int]` once `ClassVar` names a variable. that holds for the imports the
transpiler writes itself, such as the `ClassVar` an `enum class` lowers through:

```py
from typing import ClassVar
cast = _lazy_attr("typing", "cast")
```

nor is a name an **annotation** reads. whatever reads an annotation back is handed what
the name is bound to, and a proxy is not the object it stands for: `dataclasses` decides
that `_: KW_ONLY` is not a field by asking whether the annotation *is*
`dataclasses.KW_ONLY`, and `typing.get_type_hints` answers with the proxy rather than the
class. so a `from` import whose name any annotation reads stays an ordinary import — a
parameter's or a return's annotation, a variable's in a module or a class body, or one
that is a string, as every annotation is under `from __future__ import annotations`:

```by
from fractions import Fraction
from json import dumps

def show(value: Fraction) -> str:
    return dumps(str(value))
```

```py
from fractions import Fraction
dumps = _lazy_attr("json", "dumps")
```

this is an edge of the polyfill: **an import used only in annotations is not deferred on
python before 3.15**, and the module it names runs where the import is written — so one
this python does not have fails there, as an ordinary import does. an
annotation on a variable inside a function body is never evaluated, so it does not count.
on 3.15 and later the `lazy` keyword binds the real object, and such an import is deferred
like any other

the functional forms build annotations out of values instead: `NamedTuple("P", [("x", T)])`,
`TypedDict("TD", {"a": T})` and `dataclasses.make_dataclass(...)` are handed their types as
arguments, when the call runs. a name imported with `from` and read only there is read as a
value, so under the polyfill it is still a proxy, and whatever reads those annotations back is
handed the proxy: `typing.get_type_hints(P)["x"]` is the proxy rather than the class, and
`make_dataclass` makes a field of the `_` in this `Row`, and leaves `extra` positional:

```by
from dataclasses import KW_ONLY, make_dataclass

Row = make_dataclass("Row", [("name", str), ("_", KW_ONLY), ("extra", int)])
```

reach such a name through its module — `import dataclasses` and `dataclasses.KW_ONLY` — which
binds the real module rather than a proxy, or target `--min-version 3.15`

two things a proxy cannot emulate, and which are therefore limitations of the
polyfill only:

| expression                   | result                                          |
| ---------------------------- | ----------------------------------------------- |
| `type(x)`                    | `_LazyAttr`, not the value's real type          |
| `x === y` (identity)         | compares proxy identity, not value identity     |
| `x === None` on imported `x` | always `False`, even when the value *is* `None` |

the `=== None` case is the sharp one, because identity-against-a-singleton is a
common idiom: `from cfg import SENTINEL` then `SENTINEL === None` is `False`
even when `SENTINEL` is `None`, because `===` (python `is`) sees the proxy, not
the value it stands for. test imported values for emptiness by *value* — `x`'s
truthiness, `x == None`, `isinstance(x, T)` — all of which the proxy forwards.
or target `--min-version 3.15`, where the `lazy` keyword binds the real object
and none of these limitations exist
