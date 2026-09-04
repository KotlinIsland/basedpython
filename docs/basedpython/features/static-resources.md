# static resources

a json, toml or yaml file can be imported by path, which binds its document to a
name with a type:

```yaml
# data/config.yaml
a:
    b:
        - 1
        - 2
```

```by
import "data/config.yaml" as config

config.a.b[1]  # 2
```

the file is read while the program is built, and the document is written into
the module that imported it:

```python
from typing import Final


class config:
    class a:
        b: Final = (1, 2)
```

nothing is opened at run time, and nothing has to be installed to read yaml or
toml — by the time the program runs, the document is python.

## what a document becomes

a mapping becomes a class, so its keys are attributes. a sequence becomes a
tuple, so an index reaches one element rather than the union of everything in
the collection. a scalar keeps the value it was written with:

```json
{ "name": "ty", "port": 8080, "ratio": 0.5, "debug": true, "missing": null }
```

```by
import "settings.json" as settings

reveal_type(settings.name)  # "ty"
reveal_type(settings.port)  # 8080
reveal_type(settings.ratio)  # float
reveal_type(settings.debug)  # True
reveal_type(settings.missing)  # None
```

because a sequence is a tuple, an index that is not there is an error rather
than a surprise at run time:

```by
settings.ports[7]  # error: index-out-of-bounds
```

and because every value is `final`, so is the document:

```by
settings.port = 9000  # error: invalid-assignment
```

a mapping at the top of the document is the value itself. anything else — a
sequence, a scalar — is bound as it is:

```json
[80, 443]
```

```by
import "ports.json" as ports

ports[0]  # 80
```

## the path

the path is relative to the file that imports it, and it is written with `/`
whichever platform the build runs on:

```by
import "data/config.yaml" as config
import "../shared/defaults.toml" as defaults
```

an absolute path is [`invalid-static-resource`](#errors): it names a place on
one machine, and a program is not built on one machine.

the name to bind has to be written. a path is not a name, so there is nothing
for `import "data/config.yaml"` to fall back on, and it is an error to leave the
`as` clause off.

a resource is imported by a statement of its own — not beside a module import,
and not [lazily](lazy-imports.md), since there is nothing left to defer.

## keys python cannot name

a document is read through attributes, so a key that is not a valid python
identifier has no attribute to be read through. such a key is left out of the
value, and the import reports
[`unusable-resource-key`](#errors):

```json
{ "build-backend": "hatchling.build", "root": "." }
```

```by
import "pyproject.json" as project  # warning: unusable-resource-key

project.root  # "."
project.build_backend  # error: unresolved-attribute — the key is `build-backend`
```

the same goes for a name with two leading underscores: python mangles `__x`
inside a class body, so the attribute a reader would write is not the one that
would exist, and `__x__` would collide with what a class carries of its own. two
names the rendering needs for itself are left out as well — `Final`, which the
values are annotated with, and anything beginning with `_by_`.

the document still holds those keys. nothing in the program can reach them.

## the formats

| extension        | notes                                                         |
| ---------------- | ------------------------------------------------------------- |
| `.json`          | an integer too large for 64 bits is read as a float           |
| `.toml`          | a date or time is read as the text it was written with        |
| `.yaml` / `.yml` | one document per file; an anchor is expanded where it is used |

a key written twice is read as its last value, which is what json and yaml
themselves do. a yaml mapping key that is not a string is an error: a document
is read through its keys, and there would be nothing to call that one.

## two importers, two objects

the document is written into each module that imports it, so two modules
importing one file get two objects. they hold equal values and answer every
attribute the same way, but they are not the same object:

```by
# a.by
import "data/config.yaml" as config

# b.by
import "data/config.yaml" as config

a.config.a === b.config.a  # False
```

## errors

| diagnostic                | when                                                                                          |
| ------------------------- | --------------------------------------------------------------------------------------------- |
| `invalid-static-resource` | the path names nothing, is absolute, is not a resource format, or the document cannot be read |
| `unusable-resource-key`   | a key in the document has no name python can spell                                            |

reading a document that cannot be parsed also fails the build: there is no value
to write into the module.

## limits

a static resource is a document, not a module, so python read back as
basedpython never turns a class tree into one — the classes come back as
classes.

going to the definition of a value lands on the resource file rather than on the
line the key is written on.
