# packages without `__init__.by`

below `src/`, a directory holding a basedpython module is a package. it needs no
`__init__.by`, and the build writes the `__init__.py` python needs:

```text
src/app/main.by
src/app/util/text.by
```

```by
# src/app/main.by
from app.util.text import shout
```

`by build` writes:

```text
build/app/__init__.py
build/app/main.py
build/app/util/__init__.py
build/app/util/text.py
```

both initializers are empty. `app` and `app.util` are regular packages to the
checker and to python alike, and a wheel ships `app`. `by run` and `by compile`
write them into their trees too, and `by transpile <dir>` writes them beside the
python it puts in the source tree

## which directories are packages

a directory is a package when it is below a package root and holds a basedpython
module at any depth: a `.by` or `.byi` file whose name python can import, reached
through directories whose names it can import

so in

```text
src/app/core/engine.by
src/app/static/logo.png
src/my-data/table.by
src/tools/run.py
tests/test_engine.by
```

`app` and `app.core` are packages. `app.static` holds no module and is left as it
is. `my-data` is not a name python can import. `tools` holds only python, and
keeps python's meaning: it is a namespace package. `tests` is not below a package
root, so it is a folder of test modules, as it is in python, and runs by path

a package root itself is never a package, and neither is anything installed into
the environment: only the project's own directories are built

## package roots

the package root is `src/` unless
[`environment.package-roots`](../configuration.md) says otherwise. each one has to
be a module root: `src/` and the project root are, unless `environment.root` names
others. a directory is measured from the deepest module root holding it, so a
project without a `src/` directory names its own root:

```toml
[tool.basedpython.environment]
package-roots = ["."]
```

every directory of that project holding a basedpython module is then a package,
`tests/` and `scripts/` included. the linter reads the same setting, so the two
agree on which directories are packages. an entry that is not a module root is
reported as `invalid-package-root`

## writing an `__init__.by` anyway

a package can still have one. it is the module that `import app` runs, so it is
where a package's [exports](export-imports.md) and its
[module api rules](module-api.md) go:

```by
# src/app/__init__.by
from .core.engine export Engine
```

an `__init__.by` with nothing in it says nothing a package without one does not.
outside a package root, a directory still needs one to be a regular package

## namespace packages

a namespace package is a package python assembles from every directory of its
name on the path, as it does for a plugin directory several distributions add
to. a directory below a package root declared in
[`environment.namespace-packages`](../configuration.md) stays one:

```toml
[tool.basedpython.environment]
namespace-packages = ["src/acme/plugins"]
```

the build writes no initializer for it. only the directory named is declared: a
directory below it holding a module is a package of its own, as each
distribution's part of a namespace package usually is. an entry that is not a
directory below a package root is reported as `invalid-namespace-package`

## how it differs from python

in python a directory without an `__init__.py` is a namespace package, which
behaves differently from a regular one:

- **it does not shadow.** a regular package found anywhere on the path wins over
    a namespace package found earlier, so with `src/app/` and an installed
    `app/__init__.py`, python imports the installed one. a basedpython `app` is
    the project's
- **it does not outrank a module.** python looks for a package with an
    initializer before a module of the same name, so `src/app/` holding a module
    is what `import app` runs, not a `src/app.by` beside it
- **`__file__` is `None`.** for a basedpython package it is the path of the
    `__init__.py` the build wrote

## converting python

[`by transpile --reverse`](../getting-started.md#converting-python-to-basedpython)
keeps a project's packages what they were. below a package root, an empty
`__init__.py` converts to nothing when the directory is a package without it, and
is kept when it is all the directory holds. a directory with no `__init__.py`
that would become a package is declared in `environment.namespace-packages`, in
the project's `basedpython.toml`, `ty.toml` or `pyproject.toml`. a stub-only
`__init__.pyi` is kept: it makes the directory a package to a type checker alone
