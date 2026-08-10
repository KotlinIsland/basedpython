## What it does

Checks for imports, from code the project ships, of a distribution declared only
in a dependency group.

## Why is this bad?

A dependency group is for the people working on the project, not the people
installing it. Nothing installs one alongside the project, so an import of one
from shipped code raises `ModuleNotFoundError` for every user — and never for the
person who wrote it, whose environment has the whole group.

## Examples

```toml {data-mdtest="ignore"}
[project]
name = "my-lib"
dependencies = ["requests"]

[dependency-groups]
dev = ["pytest"]
```

```python {data-mdtest="ignore"}
# src/my_lib/fixtures.py
import pytest  # error: [misplaced-dependency]
```

The same import from `tests/` is fine: tests are not shipped.

## Configuration

Which files ship is derived from the name the project gives itself — a project
named `my-lib` ships the module `my_lib` — and can be stated outright when that
is not right:

```toml {data-mdtest="ignore"}
[tool.ty.analysis]
shipped-modules = ["my_lib", "my_lib_plugins"]
```

A file's groups can also be set directly, which overrides the derivation:

```toml {data-mdtest="ignore"}
[[tool.ty.overrides]]
include = ["src/my_lib/_dev_only/**"]

[tool.ty.overrides.analysis]
dependency-groups = ["*"]
```

A project that does not name itself ships nothing this check can identify, and
nothing is reported.
