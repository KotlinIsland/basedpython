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
in the environment because something else pulled it in, and the report says which declared
dependency that was — read from the install metadata, so it names a real requirement rather than a
guess

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

## what a library hands out

part of a library's interface can be made of another distribution: `pandas` hands you numpy
arrays, and nobody using pandas chose numpy. a library says which of its dependencies are part of
what it hands out, and then a project that depends on it may import those without declaring them
itself

```toml
# the library's pyproject.toml
[project]
name = "my-lib"
dependencies = ["numpy"]

[tool.basedpython.analysis]
exported-dependencies = ["numpy"]
```

`by build` writes that into the `by.typed` marker inside the built package, because that is what
the library's users have — a `pyproject.toml` is not installed alongside a package, and the marker
is

two limits keep the claim honest. a distribution can only export what it depends on itself, so
naming something unrelated says nothing. and the permission travels one link: `fastapi` exporting
`starlette` does not hand you `starlette`'s dependencies unless `starlette` exports them in turn

an export is also only as available as the dependency that makes it. what a dependency group hands
out is still a dependency group's, so shipped code cannot reach it

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
action per group the project declares

in a project uv manages — one with a `uv.lock`, its own or its workspace's — the action runs
`uv add`, and says so:

```text
Run `uv add numpy`
Run `uv add --group dev numpy`
```

that installs it as well as declaring it, which is what an import that resolves to nothing needs:
a name written into `pyproject.toml` doesn't put anything in the environment. what uv prints goes
to the editor's log, and the project is re-read when it finishes, so the warning goes away on its
own

anywhere else the action edits `pyproject.toml` directly, adding the requirement to the list.
formatting and comments elsewhere in the file are left alone

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
