# packaging

a basedpython project builds into an ordinary python wheel. name the build
backend in `pyproject.toml`:

```toml
[build-system]
requires = ["basedpython"]
build-backend = "basedpython.build"
```

and build it:

```sh
uv build
```

that produces a wheel and a source distribution in `dist/`, publishable with
`uv publish` and installable by anyone, whether or not they have ever heard of
basedpython

## starting from scratch

`by init` writes a project already shaped this way:

```sh
by init my-library --lib
```

```text
my-library/pyproject.toml
my-library/.python-version
my-library/README.md
my-library/src/my_library/__init__.by
```

leave off `--lib` and you also get a `main.by` and a configured entry point, so
`by run` works immediately

## what a build produces

`by build` writes the project to `out/` as python. that is the whole project,
not only its `.by` files:

```text
src/app/main.by        ->  out/app/main.py
src/app/helper.py      ->  out/app/helper.py
src/app/settings.json  ->  out/app/settings.json
src/app/py.typed       ->  out/app/py.typed
```

a `.by` file is transpiled; everything else is carried across unchanged, to the
same place. the one rearrangement is the source root — `src/app/main.by` is the
module `app.main`, so it lands at `app/main.py` and not at `src/app/main.py`

`out/` is a mirror, not a pile: what a previous build wrote and this one did not
is deleted, so a module you renamed does not go on being importable

a stub stays a stub. `a.byi` builds to `a.pyi`, never to `a.py`

### two sources, one module

`main.by` and a hand-written `main.py` are both the module `main`, and a build
that quietly picked one would disagree with what python imports. so it says so:

```text
`main.by` and `main.py` both build to `main.py` — they are the same module, so
one of them has to be renamed
```

## what a wheel carries

the wheel holds the transpiled python, the `.by` sources beside it, and a
`by.typed` marker in each top-level package:

```text
app/main.py
app/main.by
app/by.typed
```

python only ever imports the `.py`. the `.by` is there for the next basedpython
project along — see [depending on a basedpython
library](#depending-on-a-basedpython-library)

to ship python only:

```toml
[tool.basedpython.build]
sources = false
```

the marker still goes out either way. without the sources there is no `.by` for a
consumer to prefer, but the marker's contents are what declare which of this
project's dependencies are part of its interface — see
[declared dependencies](features/dependencies.md)

## choosing what goes in

`build.exclude` keeps files out, `build.include` narrows to a subset, and
exclusions win over inclusions:

```toml
[tool.basedpython.build]
exclude = ["tests", "**/*.snapshot"]
```

`src.exclude` already bounds the build — a file the project excludes from itself
is not part of what it ships — so `build.exclude` is for the things that belong
in the project but not in the artifact

caches, virtual environments, version-control directories, `target`, and the
output directory itself are excluded to begin with — by `src.exclude`, so a negation
there takes them back for the build too:

```toml
[tool.basedpython.src]
exclude = ["!dist"]
```

## a version that lives in the source

declare it dynamic and say where to read it from:

```toml
[project]
dynamic = ["version"]

[tool.basedpython.build]
version-from = "src/app/__init__.by"
```

```by
__version__ = "1.4.0"
```

## depending on a basedpython library

a python project depends on a basedpython library the way it depends on any
other. nothing about the dependency is unusual: it is python in the wheel

a *basedpython* project gets more. the `.by` sources travel with the wheel, and
the `by.typed` marker beside them says they are the authoritative surface — so
the declarations that have no python spelling survive the trip:

```by
extension FlowContent:
    def card(self) -> Div

def load(path: str) -> Config raises ParseError
```

a consumer reading only the transpiled python sees `load` returning a `Config`.
a consumer reading the `.by` sees that it raises, and that `card` is available
on every `FlowContent`

the marker is per package, and inherited by everything under it. it is the same
bargain [`py.typed`](https://peps.python.org/pep-0561/) strikes for inline
annotations: the package declares its own sources authoritative, rather than a
checker guessing

## editable installs

```sh
uv sync
```

installs the project pointing at `out/`, so `by build` is what refreshes an
editable install. run it after editing, the same way any compiled language
rebuilds before its changes are visible

## a single-module project

a wheel needs at least one importable package. a project whose only module is
`app.by` at the top level has nothing to package, and the build says so — move
it to `app/__init__.by` and it builds

## running on the right python

`by run` uses the project environment: the same interpreter `by check` resolves
imports against, which for a uv project is `.venv`. `$PYTHON` overrides it, and
`by run --python` overrides that

a project that targets a newer python than the interpreter can run is reported
before anything executes:

```text
this project targets python 3.13, but the interpreter this would run on is 3.9
```
