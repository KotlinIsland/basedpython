# Packages that carry their basedpython sources

A basedpython library ships both halves of itself: the transpiled `.py` that python imports, and the
`.by` those were transpiled from. Only the `.by` still says the things python has no spelling for,
so for type checking it is the better of the two — but which one is authoritative is the package's
claim to make, not the checker's guess. A `by.typed` marker beside them is how a package makes it,
in the same spirit as `py.typed`.

Without the marker, python wins.

## Without a marker, the python is the module

```toml
[environment]
extra-paths = ["/packages"]
```

`/packages/shipped/__init__.py`:

```py
```

`/packages/shipped/value.py`:

```py
NUMBER: int = 1
```

`/packages/shipped/value.by`:

```by
NUMBER: str = "from the source"
```

```py
from shipped.value import NUMBER

reveal_type(NUMBER)  # revealed: int
```

## With a marker, the basedpython source is

```toml
[environment]
extra-paths = ["/packages"]
```

`/packages/shipped/by.typed`:

```text
```

`/packages/shipped/__init__.py`:

```py
```

`/packages/shipped/value.py`:

```py
NUMBER: int = 1
```

`/packages/shipped/value.by`:

```by
NUMBER: str = "from the source"
```

```py
from shipped.value import NUMBER

reveal_type(NUMBER)  # revealed: str
```

## The package's own `__init__` is covered by its marker

The marker sits beside `__init__`, so it has to be read before `__init__` itself resolves —
otherwise the one module every import of the package touches is the one module the marker does not
reach.

```toml
[environment]
extra-paths = ["/packages"]
```

`/packages/shipped/by.typed`:

```text
```

`/packages/shipped/__init__.py`:

```py
NAME: int = 1
```

`/packages/shipped/__init__.by`:

```by
NAME: str = "from the source"
```

```py
from shipped import NAME

reveal_type(NAME)  # revealed: str
```

## A marker is inherited by subpackages

A package declares it once, at the top. Every module underneath is part of that same distribution
and was shipped by the same build.

```toml
[environment]
extra-paths = ["/packages"]
```

`/packages/shipped/by.typed`:

```text
```

`/packages/shipped/__init__.py`:

```py
```

`/packages/shipped/inner/__init__.py`:

```py
```

`/packages/shipped/inner/deep.py`:

```py
NUMBER: int = 1
```

`/packages/shipped/inner/deep.by`:

```by
NUMBER: str = "from the source"
```

```py
from shipped.inner.deep import NUMBER

reveal_type(NUMBER)  # revealed: str
```

## A stub still outranks both

The marker settles which *source* is authoritative. It says nothing about stubs, which outrank
source either way.

```toml
[environment]
extra-paths = ["/packages"]
```

`/packages/shipped/by.typed`:

```text
```

`/packages/shipped/__init__.py`:

```py
```

`/packages/shipped/value.pyi`:

```pyi
NUMBER: bytes
```

`/packages/shipped/value.py`:

```py
NUMBER: int = 1
```

`/packages/shipped/value.by`:

```by
NUMBER: str = "from the source"
```

```py
from shipped.value import NUMBER

reveal_type(NUMBER)  # revealed: bytes
```

## A marker loose outside a package claims nothing

A marker only speaks for the package it is in. Left directly in a search path it would otherwise
re-point every top-level module in the environment, which is not a claim any one package has any
business making for its neighbours.

```toml
[environment]
extra-paths = ["/packages"]
```

`/packages/by.typed`:

```text
```

`/packages/loose.py`:

```py
NUMBER: int = 1
```

`/packages/loose.by`:

```by
NUMBER: str = "from the source"
```

```py
from loose import NUMBER

reveal_type(NUMBER)  # revealed: int
```
