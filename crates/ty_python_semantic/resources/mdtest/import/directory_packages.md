# Packages without an initializer

A first-party directory holding a basedpython module is a regular package, with or without an
`__init__.by`. The build writes the empty `__init__.py` python needs, so it is a regular package at
runtime too.

## A regular package shadows a later one

A regular package is the only package of its name: python looks no further down the path for more of
it. So the project's `pkg` hides the installed one, and `pkg.b` is not found.

```toml
[environment]
python = "/.venv"
```

`pkg/a.by`:

```by
a = 1
```

`/.venv/<path-to-site-packages>/pkg/__init__.py`:

```py
```

`/.venv/<path-to-site-packages>/pkg/b.py`:

```py
b = 2
```

`main.by`:

```by
import pkg.a
import pkg.b  # error: [unresolved-import]
```

## A package outranks a module of its name

Python looks for a package with an initializer before a module of the same name, and the build gives
the directory one, so `import pkg` runs the package and not `pkg.by`.

`pkg.by`:

```by
x: int = 1
```

`pkg/a.by`:

```by
a = 1
```

`main.by`:

```by
import pkg
import pkg.a

pkg.x  # error: [unresolved-attribute]
```

## `__file__` is a string

The package's `__file__` is the path of the initializer the build wrote, where a namespace package's
is `None`.

`pkg/a.by`:

```by
a = 1
```

`main.by`:

```by
import pkg

reveal_type(pkg.__file__)  # revealed: str
```

## A module at any depth makes a package

Each directory on the way down to a basedpython module is a package.

`outer/inner/a.by`:

```by
a = 1
```

`main.by`:

```by
import outer
import outer.inner

reveal_type(outer.__file__)  # revealed: str
reveal_type(outer.inner.__file__)  # revealed: str
```

## A directory of python stays a namespace package

A directory holding no basedpython module keeps python's meaning, so its portions on different
search paths are merged.

```toml
[environment]
python = "/.venv"
```

`ns/one.py`:

```py
one = 1
```

`/.venv/<path-to-site-packages>/ns/two.py`:

```py
two = 2
```

`main.by`:

```by
import ns
import ns.one
import ns.two  # no diagnostic

reveal_type(ns.__file__)  # revealed: None
```

## A declared namespace package

A directory named in `environment.namespace-packages` stays a namespace package, so it is merged
with the installed portion of the same name. A directory below it holding a module is a package of
its own.

```toml
[environment]
python = "/.venv"
namespace-packages = ["plugins"]
```

`plugins/one/a.by`:

```by
a = 1
```

`/.venv/<path-to-site-packages>/plugins/two/__init__.py`:

```py
```

`main.by`:

```by
import plugins
import plugins.one
import plugins.two  # no diagnostic

reveal_type(plugins.__file__)  # revealed: None
reveal_type(plugins.one.__file__)  # revealed: str
```
