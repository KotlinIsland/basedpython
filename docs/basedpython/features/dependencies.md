# declared dependencies

an import only works if the code importing it depends on the thing it names. what you declare in
`pyproject.toml` says what that is, and that is checked

```toml
[project]
name = "my-lib"
dependencies = ["requests"]

[dependency-groups]
dev = ["pytest"]
```

```python
import charset_normalizer  # warning: not a declared dependency
```

`charset_normalizer` is installed — `requests` needs it — so the import works today. it stops
working the moment `requests` drops it or moves to a version that doesn't, and it stops working
for everyone who installs the project fresh, never for the person who wrote it

## what is checked

**`undeclared-dependency`** — the module comes from a distribution no group declares. it is only
in the environment because something else pulled it in

**`misplaced-dependency`** — the module comes from a distribution declared only in a dependency
group, and the import is in code the project ships. nothing installs a dependency group alongside
the project, so the import fails for every user of it

```python
# my_lib/fixtures.py
import pytest  # warning: `pytest` is in dependency group `dev`
```

the same import from `tests/` is fine. tests are not shipped

## what ships

the modules the project ships are the ones named after it: a project named `my-lib` ships `my_lib`
and everything under it. a project that doesn't name itself ships nothing that can be identified,
and then nothing is reported

state it outright when that is not right:

```toml
[tool.ty.analysis]
shipped-modules = ["my_lib", "my_lib_plugins"]
```

or set a file's groups directly, which overrides the derivation entirely:

```toml
[[tool.ty.overrides]]
include = ["src/my_lib/_dev_only/**"]

[tool.ty.overrides.analysis]
dependency-groups = ["*"]
```

`project` names `[project].dependencies`, an extra or a dependency group is named by its own name,
and `*` names every group

## an extra is not a dependency group

an extra is installed for anyone who asks for it, so shipped code may import one. whether the
import is guarded is the project's business, not this check's

## completions

the same question decides what an `import` completes to. a distribution the project only has
because something else needed it is left out of the list, and out of auto-import

it comes back as soon as you name it — `import charset_no` completes — because at that point you
are asking for it rather than browsing

## adding a dependency

an editor offers a quick fix on both checks, and on an import that resolves to nothing at all: one
action per group the project declares, each adding the requirement to `pyproject.toml`. formatting
and comments elsewhere in the file are left alone

for a module that isn't installed, the distribution's name can only be guessed from the module's —
right most of the time, and wrong for the likes of `yaml`, which `PyYAML` installs. for one that
is installed, the name comes from the install metadata and is exact

## where the declaration is read from

`[project].dependencies`, `[project.optional-dependencies]`, PEP 735 `[dependency-groups]`
(including `include-group`), `[tool.uv].dev-dependencies`, and the PEP 723 metadata block of a
single-file script

nothing is reported when there is nothing to read: a project with no `pyproject.toml`, an
environment ty cannot attribute to distributions, and a module no distribution installed all mean
the same thing — nothing is known, so nothing is out of place
