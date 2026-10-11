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
so the polyfill lowers the two forms differently

`import os` binds a real module object whose body runs on first attribute access:

```python
os = _lazy_module("os")
```

nothing stands in for the module, so this form behaves exactly like an eager
import apart from when the body runs

`from json import dumps` binds nothing where it is written. every read of `dumps`
is written as a read through one object the module keeps for its deferred names,
which resolves the name on first use:

```by
from json import dumps

def show(value: int) -> str:
    return dumps(value)
```

transpiles to:

```python
_by_lazy = _LazyFrom(globals(), ("dumps",))
__getattr__ = _by_lazy.__by_getattr__
__dir__ = _by_lazy.__by_dir__
_lazy_from(_by_lazy, "json", {"dumps": "dumps"})

def show(value: int) -> str:
    return _by_lazy.dumps(value)
```

(the runtime the polyfill calls is left out here)

the first read of `dumps` imports `json`, reads `json.dumps` and stores it in the
module's namespace under `dumps`, and every read after that finds it there. so
the name evaluates to the object itself — `type(dumps)`, `dumps === json.dumps`,
handing it to `json.dumps` or `pytest.approx`, all answer as they would for an
ordinary import

### what other modules see

the module serves the name to other modules as an ordinary import would:
`from app import dumps`, `app.dumps`, `hasattr(app, "dumps")` and `dir(app)` all
find it, through the module's `__getattr__` and `__dir__`

a patch of the name where it is used — `mock.patch("app.dumps")`,
`monkeypatch.setattr(app, "dumps", fake)` — writes the module's namespace, which
is where every read in the module looks first, so the module's own code sees the
patch, and sees the original again once it is undone

### the name is resolved once

a name is read off its module at its first use and kept, as PEP 810 does it. an
ordinary `from` import reads it when the import runs instead, so a module that
rebinds `json.dumps` between the import and the first use is seen by the polyfill
and not by an ordinary import. a rebinding after the first use is seen by neither

### until the first use, the name is not in the namespace

nothing is bound where the import is written, so until something reads the
name, it is not in the module's namespace:

| expression                                 | before the first use    |
| ------------------------------------------ | ----------------------- |
| `globals()["dumps"]`, `vars(app)["dumps"]` | `KeyError`              |
| `from app import *`, with no `__all__`     | does not import `dumps` |
| `eval("dumps")` in the module              | `NameError`             |
| `typing.ForwardRef("dumps")`, evaluated    | `NameError`             |

anything that walks a module's `__dict__` rather than asking for a name misses it
too: pytest does not collect a test class a test module imports and never uses

reading the name once puts it there, and a module with an `__all__` that lists it
passes it on to `from app import *` whether it was read or not

a read made before the import statement has run — or where it never ran, as in
`if PY2: from itertools import izip as zip` — finds what python's lookup finds: the
builtin of that name if there is one, and a `NameError` if not

a reload runs the module again, and each of its `from` imports reads its name off the
module again on its next use, as an ordinary import reads it again when it runs

### what stays an ordinary import

the polyfill defers a name only where every read of it can be rewritten, and
where nothing reads it by identity before it is used. a name it does not defer
keeps an ordinary import, and the other names of the statement are still
deferred:

```py
from json import JSONDecodeError; _lazy_from(_by_lazy, "json", {"dumps": "dumps"})
```

- **an exception class.** a deferred name is resolved where it is first read, and
    for an exception class that is a handler: an import failing there would
    surface while another exception is being handled, and replace it
- **a special form** — `ClassVar`, `Final`, `Literal`, `Annotated`, `Protocol` and
    the rest of what a type checker gives a meaning of its own. what reads one
    tells it apart by identity: `dataclasses` decides a bare `ClassVar` annotation
    is not a field by asking whether it *is* `typing.ClassVar`, and a checker
    reading the emitted python rejects `ClassVar[int]` once `ClassVar` is anything
    else. that holds for the imports the transpiler writes itself, such as the
    `ClassVar` an `enum class` lowers through
- **a name an annotation reads.** whatever reads an annotation back reads its names
    from the module's namespace: `typing.get_type_hints` evaluates a string
    annotation against it, and `dataclasses` decides that `_: KW_ONLY` is not a
    field by asking whether the annotation *is* `dataclasses.KW_ONLY`. that counts a
    parameter's or a return's annotation, a variable's in a module or a class
    body, and one that is a string, as every annotation is under
    `from __future__ import annotations`. an annotation on a variable inside a
    function body is never evaluated, so it does not count
- **a name a forward reference the transpiler writes reads** — a type parameter's
    bound written ahead of the class it reads a name of, as
    `TypeVar("T", bound="dict[Thing, C.Inner]")`: whatever evaluates that string
    looks the name up in the module's namespace. a string in a `.by` type expression
    is a [literal type](literal-types.md) and is written as `Literal["Thing"]`, so it
    names nothing; a string handed to `typing.ForwardRef` by hand or to `eval` is not
    seen, and is the last two rows of the table above
- **a name a decorator reads.** a decorator says what the definition under it
    *is*, and a checker reading the emitted python recognizes `@overload` or
    `@dataclass` only by name. deferring one buys nothing either: a decorator is
    applied while the module is executing
- **a name bound again anywhere in the module** — assigned, deleted, a `def` or a
    `class` of that name, a `for`, `with`, `except` or walrus target, a second
    import, or a `global` declaration that binds it from a function. a read could
    find either binding, and only the one the import makes is deferred
- **a name a class body reads where the body may have bound it already**: the read
    finds the class's binding on some paths and the import on others
- **a name read where python keeps the text of the read**: `f"{dumps=}"` prints
    the expression as written, and a t-string's `Interpolation` carries it
- **a name beginning with two underscores**, which a class body mangles
- **every name of a module that has a `from … import *`**, which can bind any
    name, **or its own `__getattr__` or `__dir__`**, which the deferred names are
    served through
- **every name of a module compiled with [`by compile`](../cli-reference.md#by-compile)**,
    whose compiled functions read the module's globals by name. its module imports
    are still deferred

```by
from fractions import Fraction
from json import dumps

def show(value: Fraction) -> str:
    return dumps(str(value))
```

```py
from fractions import Fraction
_lazy_from(_by_lazy, "json", {"dumps": "dumps"})
```

this is an edge of the polyfill: **an import used only in annotations is not deferred on
python before 3.15**, and the module it names runs where the import is written — so one
this python does not have fails there, as an ordinary import does. on 3.15 and later the
`lazy` keyword binds the real object, and such an import is deferred like any other
