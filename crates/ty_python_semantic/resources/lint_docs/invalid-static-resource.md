## What it does

Checks for basedpython static resource imports that cannot be read.

## Why is this bad?

`import "data/config.yaml" as config` says the file is part of the program. A
path that names nothing, a path that names a place on one machine, a file in a
format that is not `.json`, `.toml`, `.yaml` or `.yml`, and a document the
format's own parser rejects all leave the import with no value to bind.

## Examples

`main.by`:

```by
# error: [invalid-static-resource]
import "data/config.txt" as config

# error: [invalid-static-resource]
import "/etc/hosts.json" as hosts

# error: [invalid-static-resource]
import "data/missing.json" as missing
```
