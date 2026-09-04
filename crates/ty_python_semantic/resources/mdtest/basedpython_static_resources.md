# Static resources

A json, toml or yaml file can be imported by path, which binds its document to a name.

## A mapping is read through its keys

The document's mapping keys are the value's attributes, all the way down. A sequence is a tuple, so
an index reaches one element rather than the union of all of them.

`data/config.yaml`:

```yaml
a:
    b:
        - 1
        - 2
```

`main.by`:

```by
import "data/config.yaml" as config

reveal_type(config.a.b[1])  # revealed: 2
reveal_type(config.a.b)  # revealed: (1, 2)
```

## Scalars keep the value they were written with

`data/settings.yaml`:

```yaml
name: ty
port: 8080
ratio: 0.5
debug: true
missing: ~
```

`main.by`:

```by
import "data/settings.yaml" as settings

reveal_type(settings.name)  # revealed: "ty"
reveal_type(settings.port)  # revealed: 8080
reveal_type(settings.ratio)  # revealed: float
reveal_type(settings.debug)  # revealed: True
reveal_type(settings.missing)  # revealed: None
```

## An index past the end of a sequence is an error

`data/ports.json`:

```json
{ "ports": [80, 443] }
```

`main.by`:

```by
import "data/ports.json" as config

reveal_type(config.ports[1])  # revealed: 443
# error: [index-out-of-bounds]
reveal_type(config.ports[2])  # revealed: Unknown
```

## A key the document does not have is an error

`data/config.json`:

```json
{ "a": 1 }
```

`main.by`:

```by
import "data/config.json" as config

# error: [unresolved-attribute]
reveal_type(config.b)  # revealed: Unknown
```

## A value cannot be assigned to

The document is fixed at build time, so its attributes are `Final`.

`data/config.json`:

```json
{ "a": 1 }
```

`main.by`:

```by
import "data/config.json" as config

config.a = 2  # snapshot
```

```snapshot
error[invalid-assignment]: Cannot assign to final attribute `a` on type `<class 'config'>`
 --> src/main.by:3:1
  |
3 | config.a = 2  # snapshot
  | ^^^^^^^^ `Final` attributes can only be assigned in the class body or `__init__`
```

## toml

`data/config.toml`:

```toml
[server]
host = "localhost"
ports = [80, 443]
```

`main.by`:

```by
import "data/config.toml" as config

reveal_type(config.server.host)  # revealed: "localhost"
reveal_type(config.server.ports[0])  # revealed: 80
```

## A mapping inside a sequence

`data/servers.json`:

```json
{ "servers": [{ "host": "a" }, { "host": "b" }] }
```

`main.by`:

```by
import "data/servers.json" as config

reveal_type(config.servers[1].host)  # revealed: "b"
```

## A document that is not a mapping

The top of a document does not have to be a mapping; the name is then bound to whatever is there.

`data/ports.json`:

```json
[80, 443]
```

`main.by`:

```by
import "data/ports.json" as ports

reveal_type(ports[0])  # revealed: 80
```

## The path is relative to the importing file

`pkg/data/config.json`:

```json
{ "a": 1 }
```

`pkg/inner/main.by`:

```by
import "../data/config.json" as config

reveal_type(config.a)  # revealed: 1
```

## A path that names nothing

`main.by`:

```by
# error: [invalid-static-resource] "Cannot read static resource `data/missing.json`"
import "data/missing.json" as config

reveal_type(config)  # revealed: Unknown
```

## A file that is not a resource format

`data/notes.txt`:

```text
hello
```

`main.by`:

```by
# error: [invalid-static-resource] "Cannot read static resource `data/notes.txt`"
import "data/notes.txt" as notes
```

## An absolute path

A resource is part of the program, so it is named the way the program is laid out rather than the
way one machine is.

`main.by`:

```by
# error: [invalid-static-resource] "Cannot read static resource `/etc/config.json`"
import "/etc/config.json" as config
```

## A document that cannot be read

`data/broken.json`:

```json
{ "a": }
```

`main.by`:

```by
# error: [invalid-static-resource] "Cannot read static resource `data/broken.json`"
import "data/broken.json" as config
```

## A key python cannot name

Such a key is left out of the value: there is no attribute it could be read through.

`data/config.json`:

```json
{ "build-backend": "hatchling.build", "root": "." }
```

`main.by`:

```by
# error: [unusable-resource-key] "Key `build-backend` of `data/config.json` cannot be read as an attribute"
import "data/config.json" as project

reveal_type(project.root)  # revealed: "."
```

## A resource is imported by a statement of its own

A module import is a name the runtime goes and resolves; a resource is a document written into the
program. The two do not share a statement.

`data/config.json`:

```json
{ "a": 1 }
```

`main.by`:

```by
# error: [invalid-syntax] "A static resource is imported by a statement of its own"
import os, "data/config.json" as config
```

## A resource cannot be lazy

`data/config.json`:

```json
{ "a": 1 }
```

`main.by`:

```by
# error: [invalid-syntax] "A static resource is read while the program is built, so it cannot be lazy"
lazy import "data/config.json" as config
```

## A resource has to say what it binds

A path is not a name, so there is nothing for the import to fall back on.

`data/config.json`:

```json
{ "a": 1 }
```

`main.by`:

```by
# error: [invalid-syntax] "Expected `as` and a name to bind the static resource to"
import "data/config.json"
```

## Names the rendering needs for itself are left out too

`Final` is what the values are annotated with, and `_by_…` is what the classes a document needs for
itself are called. A key of either name would be what a sibling resolves to, so neither is exposed.
Nor is a name python would mangle inside a class body.

`data/reserved.json`:

```json
{ "Final": 1, "_by_reserved_0": 2, "__x": 3, "a": { "b-c": 4 }, "ok": 5 }
```

`main.by`:

```by
# error: [unusable-resource-key] "4 keys of `data/reserved.json` cannot be read as attributes"
import "data/reserved.json" as reserved

reveal_type(reserved.ok)  # revealed: 5
```

## A key written twice reads as its last value

`data/twice.json`:

```json
{ "a": 1, "b": 2, "a": 3 }
```

`main.by`:

```by
import "data/twice.json" as twice

reveal_type(twice.a)  # revealed: 3
reveal_type(twice.b)  # revealed: 2
```

## A string keeps what it holds

`data/text.json`:

```json
{ "quoted": "he said \"hi\"", "lines": "a\nb", "unicode": "héllo" }
```

`main.by`:

```by
import "data/text.json" as text

reveal_type(text.quoted)  # revealed: "he said \"hi\""
reveal_type(text.lines)  # revealed: "a\nb"
reveal_type(text.unicode)  # revealed: "héllo"
```

## A document too deep to read

Counted as the document is read, so a file that nests past any stack is a diagnostic rather than a
crash.

`data/deep.yaml`:

```yaml
a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a:
  { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a: { a:
  { a: 1 } } } } } } } } } } } } } } } } } } } } } } } } } } } } } } } }
```

`main.by`:

```by
# error: [invalid-static-resource] "Cannot read static resource `data/deep.yaml`"
import "data/deep.yaml" as deep
```

## Only basedpython has static resources

`main.py`:

```py
# error: [invalid-syntax] "Expected one or more symbol names after import"
# error: [invalid-syntax] "Expected a statement"
# error: [unresolved-reference] "Name `config` used when not defined"
import "data/config.json" as config
```
