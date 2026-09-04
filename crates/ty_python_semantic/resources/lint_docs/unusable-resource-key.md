## What it does

Checks for keys in an imported static resource that python cannot name.

## Why is this bad?

A static resource is read through attributes, so a key that is not a valid python identifier —
`build-backend`, `class`, `2` — has no attribute to be read through, and is left out of the value
the import binds. The document still holds it; nothing in the program can reach it.

Names with two leading underscores are left out for the same reason: python mangles `__x` inside a
class body, so the attribute the reader would write is not the one that would exist.

## Examples

`data/project.json`:

```json
{ "build-backend": "hatchling.build", "root": "." }
```

`main.by`:

```by
# error: [unusable-resource-key]
import "data/project.json" as project

reveal_type(project.root)  # revealed: "."
```
