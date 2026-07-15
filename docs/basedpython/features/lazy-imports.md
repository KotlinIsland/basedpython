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

## target version

on python 3.15 and later, the PEP 810 `lazy` keyword is used directly.
on older runtimes, a runtime polyfill is emitted. `from __future__` and
`from x import *` are always left eager.

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
